use crate::{
    db::{self, Progress, Store},
    library::{Library, Media},
    player,
};
use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        Clear as ClearScreen, ClearType, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use nucleo_matcher::{
    Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::PathBuf,
    sync::atomic::Ordering,
    time::Duration,
};

const ACCENT: Color = Color::Cyan;

pub struct Browser {
    query: String,
    matcher: Matcher,
    searching: bool,
    episodes_focus: bool,
    extras: bool,
    groups: Vec<Group>,
    group_state: ListState,
    episode_state: ListState,
    jump_notice: Option<&'static str>,
    pub notice: String,
    pub next: Option<Media>,
}

struct Group {
    name: String,
    items: Vec<Media>,
    continuing: bool,
}

pub enum Action {
    Quit,
    Play { media: Media, restart: bool },
}

struct Session {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Session {
    fn new() -> Result<Self> {
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut session = Self { terminal };
        enable_raw_mode()?;
        execute!(
            session.terminal.backend_mut(),
            EnterAlternateScreen,
            ClearScreen(ClearType::All)
        )?;
        Ok(session)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

impl Browser {
    pub fn new(query: String) -> Self {
        Self {
            query,
            matcher: Matcher::default(),
            searching: false,
            episodes_focus: false,
            extras: false,
            groups: Vec::new(),
            group_state: ListState::default(),
            episode_state: ListState::default(),
            jump_notice: None,
            notice: String::new(),
            next: None,
        }
    }

    fn refresh(&mut self, library: &Library, history: &HashMap<PathBuf, Progress>) {
        let previous = self
            .group_state
            .selected()
            .and_then(|i| self.groups.get(i))
            .map(|g| g.name.clone());
        let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
        // The matcher's stable sort keeps titles with equal scores in library order.
        self.groups = pattern
            .match_list(library.groups.keys(), &mut self.matcher)
            .into_iter()
            .map(|(name, _)| Group {
                name: name.clone(),
                items: library.groups[name].clone(),
                continuing: false,
            })
            .collect();
        if self.query.is_empty() {
            let mut continuing: Vec<_> = library
                .items()
                .filter(|item| {
                    history
                        .get(&item.path)
                        .is_some_and(|p| p.position > 0.0 && !p.watched)
                })
                .cloned()
                .collect();
            continuing.sort_by(|a, b| {
                history[&b.path]
                    .updated
                    .cmp(&history[&a.path].updated)
                    .then(a.path.cmp(&b.path))
            });
            if !continuing.is_empty() {
                self.groups.insert(
                    0,
                    Group {
                        name: "Continue watching".to_owned(),
                        items: continuing,
                        continuing: true,
                    },
                );
            }
        }
        let selected = previous
            .and_then(|name| self.groups.iter().position(|g| g.name == name))
            .unwrap_or(0);
        self.group_state
            .select((!self.groups.is_empty()).then_some(selected));
        self.clamp_episode();
    }

    fn items(&self) -> Vec<&Media> {
        self.group_state
            .selected()
            .and_then(|i| self.groups.get(i))
            .map(|group| {
                let only_extras = group.items.iter().all(|item| item.extra);
                group
                    .items
                    .iter()
                    .filter(|item| group.continuing || item.extra == (self.extras || only_extras))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn selected(&self) -> Option<Media> {
        self.episode_state
            .selected()
            .and_then(|i| self.items().get(i).cloned().cloned())
    }

    fn clamp_episode(&mut self) {
        let length = self.items().len();
        self.episode_state.select(if length == 0 {
            None
        } else {
            Some(self.episode_state.selected().unwrap_or(0).min(length - 1))
        });
    }

    pub fn run(&mut self, library: &Library, store: &Store) -> Result<Action> {
        let mut history = store.all()?;
        self.refresh(library, &history);
        let mut session = Session::new()?;
        loop {
            session
                .terminal
                .draw(|frame| self.draw(frame, library, &history))?;
            if player::INTERRUPTED.load(Ordering::Relaxed) {
                return Ok(Action::Quit);
            }
            if !event::poll(Duration::from_millis(100))? {
                continue;
            }
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            self.jump_notice = None;
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                return Ok(Action::Quit);
            }
            if self.next.is_some() {
                match key.code {
                    KeyCode::Enter | KeyCode::Char('y') => {
                        return Ok(Action::Play {
                            media: self.next.take().unwrap(),
                            restart: false,
                        });
                    }
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('q') => self.next = None,
                    _ => {}
                }
                continue;
            }
            if self.searching {
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => self.searching = false,
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.refresh(library, &history);
                        self.episode_state.select(Some(0));
                        self.clamp_episode();
                    }
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.query.push(c);
                        self.refresh(library, &history);
                        self.episode_state.select(Some(0));
                        self.clamp_episode();
                    }
                    _ => {}
                }
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(Action::Quit),
                KeyCode::Char('/') => self.searching = true,
                KeyCode::Tab | KeyCode::BackTab => self.episodes_focus.toggle(),
                KeyCode::Left | KeyCode::Char('h') => self.episodes_focus = false,
                KeyCode::Right | KeyCode::Char('l') => self.episodes_focus = true,
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN),
                KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX),
                KeyCode::Char('n') => self.jump_unwatched(&history, false),
                KeyCode::Char('N') if self.episodes_focus => self.jump_unwatched(&history, true),
                KeyCode::Char('e') => {
                    self.extras.toggle();
                    self.episode_state.select(Some(0));
                    self.clamp_episode();
                }
                KeyCode::Char('c') => {
                    self.query.clear();
                    self.refresh(library, &history);
                    self.group_state
                        .select((!self.groups.is_empty()).then_some(0));
                    self.episode_state.select(Some(0));
                    self.clamp_episode();
                    self.episodes_focus = true;
                }
                KeyCode::Char('w') if self.episodes_focus => {
                    if let Some(media) = self.selected() {
                        let old = history.get(&media.path).cloned().unwrap_or_default();
                        let new = Progress {
                            watched: !old.watched,
                            updated: db::now(),
                            ..Progress::default()
                        };
                        store.save(&media.path, &new)?;
                        history.insert(media.path, new);
                        self.refresh(library, &history);
                    }
                }
                KeyCode::Enter if !self.episodes_focus => self.episodes_focus = true,
                KeyCode::Enter | KeyCode::Char('r') if self.episodes_focus => {
                    if let Some(media) = self.selected() {
                        return Ok(Action::Play {
                            media,
                            restart: key.code == KeyCode::Char('r'),
                        });
                    }
                }
                _ => {}
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let length = if self.episodes_focus {
            self.items().len()
        } else {
            self.groups.len()
        };
        let state = if self.episodes_focus {
            &mut self.episode_state
        } else {
            &mut self.group_state
        };
        if length == 0 {
            state.select(None);
            return;
        }
        let current = state.selected().unwrap_or(0);
        let next = current.saturating_add_signed(delta).min(length - 1);
        state.select(Some(next));
        if !self.episodes_focus && next != current {
            self.episode_state.select(Some(0));
            self.clamp_episode();
        }
    }

    fn jump_unwatched(&mut self, history: &HashMap<PathBuf, Progress>, backward: bool) {
        let items = self.items();
        let unwatched = |item: &&Media| !history.get(&item.path).is_some_and(|p| p.watched);
        let current = self
            .episode_state
            .selected()
            .filter(|_| self.episodes_focus);
        let target = if backward {
            items
                .iter()
                .take(current.unwrap_or(items.len()))
                .rposition(unwatched)
        } else {
            let start = current.map_or(0, |i| i + 1);
            items
                .iter()
                .skip(start)
                .position(unwatched)
                .map(|i| start + i)
        };

        if let Some(index) = target {
            self.episode_state.select(Some(index));
            self.episodes_focus = true;
        } else {
            self.jump_notice = Some(match (self.episodes_focus, backward) {
                (false, _) => "No unwatched videos in this list",
                (true, true) => "No unwatched videos above",
                (true, false) => "No unwatched videos below",
            });
        }
    }

    fn draw(&mut self, frame: &mut Frame, library: &Library, history: &HashMap<PathBuf, Progress>) {
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .split(frame.area());
        let search = if self.searching {
            format!("Search: {}▏", self.query)
        } else if self.query.is_empty() {
            "Your library · / to search".to_owned()
        } else {
            format!("Search: {} · / to edit", self.query)
        };
        frame.render_widget(
            Paragraph::new(search)
                .block(Block::bordered().title(" watch "))
                .style(Style::default().fg(ACCENT)),
            rows[0],
        );
        let panes = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[1]);
        let group_rows: Vec<_> = self
            .groups
            .iter()
            .map(|group| {
                let watched = group
                    .items
                    .iter()
                    .filter(|item| history.get(&item.path).is_some_and(|p| p.watched))
                    .count();
                let (title, detail) =
                    if let Some((title, season)) = group.name.split_once(" · Season ") {
                        (
                            title.to_owned(),
                            format!("Season {season} · {watched}/{} watched", group.items.len()),
                        )
                    } else {
                        (
                            group.name.clone(),
                            format!("{watched}/{} watched", group.items.len()),
                        )
                    };
                ListItem::new(vec![
                    Line::from(title),
                    Line::from(Span::styled(
                        format!("  {detail}"),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
            })
            .collect();
        let groups = List::new(group_rows)
            .block(pane(" Library ", !self.episodes_focus))
            .highlight_style(highlight(!self.episodes_focus))
            .highlight_symbol("› ");
        frame.render_stateful_widget(groups, panes[0], &mut self.group_state);
        let items = self.items();
        let continuing = self
            .group_state
            .selected()
            .and_then(|i| self.groups.get(i))
            .is_some_and(|g| g.continuing);
        let episode_rows: Vec<_> = items
            .iter()
            .map(|item| {
                let progress = history.get(&item.path).cloned().unwrap_or_default();
                let label = if continuing {
                    format!("{} · {}", item.group(), item.label)
                } else {
                    item.label.clone()
                };
                ListItem::new(vec![
                    Line::from(label),
                    Line::from(Span::styled(
                        format!("  {}", progress.status()),
                        Style::default().fg(if progress.watched {
                            Color::Green
                        } else {
                            Color::DarkGray
                        }),
                    )),
                ])
            })
            .collect();
        let title = if self.extras {
            " Specials & extras · e to show episodes "
        } else {
            " Episodes / movies · e for extras "
        };
        let episodes = List::new(episode_rows)
            .block(pane(title, self.episodes_focus))
            .highlight_style(highlight(self.episodes_focus))
            .highlight_symbol("› ");
        frame.render_stateful_widget(episodes, panes[1], &mut self.episode_state);
        if self.groups.is_empty() {
            frame.render_widget(
                Paragraph::new(if library.groups.is_empty() {
                    "No videos found. Add files to ~/Movies or use --root <directory>."
                } else {
                    "No matching titles. Press / to edit your search."
                })
                .wrap(Wrap { trim: true })
                .block(Block::bordered()),
                panes[1],
            );
        }
        let detail = self
            .selected()
            .map(|m| m.path.display().to_string())
            .unwrap_or_default();
        let warning = if self.notice.is_empty() {
            library.warnings.first().map_or("", String::as_str)
        } else {
            &self.notice
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(detail),
                Line::from(Span::styled(
                    self.jump_notice.unwrap_or(warning),
                    Style::default().fg(Color::Yellow),
                )),
            ])
            .block(Block::bordered().title(" File ")),
            rows[2],
        );
        frame.render_widget(
            Paragraph::new(concat!(
                "↑↓/jk move · Tab switch · Enter select/play · / search · e extras\n",
                "n/N unwatched · w watched · r restart · c continue · q quit"
            ))
            .style(Style::default().fg(Color::DarkGray)),
            rows[3],
        );
        if let Some(next) = &self.next {
            let area = centered(frame.area(), 64, 7);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(format!(
                    "{}\n{}\n\nEnter/y: play next    Esc/n: return to library",
                    next.group(),
                    next.label
                ))
                .block(
                    Block::bordered()
                        .title(" Play next episode? ")
                        .border_style(Style::default().fg(ACCENT)),
                )
                .wrap(Wrap { trim: true }),
                area,
            );
        }
    }
}

fn pane(title: &str, focused: bool) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused { ACCENT } else { Color::DarkGray }))
}

fn highlight(focused: bool) -> Style {
    Style::default()
        .bg(if focused {
            Color::DarkGray
        } else {
            Color::Black
        })
        .fg(if focused { ACCENT } else { Color::White })
        .add_modifier(Modifier::BOLD)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn renders_empty_and_small_terminals() {
        let mut browser = Browser::new(String::new());
        let library = Library {
            groups: Default::default(),
            warnings: vec![],
        };
        for (width, height) in [(100, 30), (20, 5), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| browser.draw(frame, &library, &HashMap::new()))
                .unwrap();
        }
    }

    #[test]
    fn search_and_extras_preserve_correct_selection() {
        let regular = crate::library::parse(PathBuf::from("Show - 02.mkv"), None);
        let extra = Media {
            extra: true,
            ..regular.clone()
        };
        let mut groups =
            std::collections::BTreeMap::from([("Show".to_owned(), vec![regular, extra])]);
        groups.extend(["A Shadow", "B Shadow"].map(|name| {
            let media = crate::library::parse(PathBuf::from(format!("{name} - 02.mkv")), None);
            (name.to_owned(), vec![media])
        }));
        let library = Library {
            groups,
            warnings: vec![],
        };
        let mut browser = Browser::new("shw".to_owned());
        browser.refresh(&library, &HashMap::new());
        assert_eq!(
            browser
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            ["Show", "A Shadow", "B Shadow"]
        );
        assert_eq!(browser.items().len(), 1);
        assert!(!browser.selected().unwrap().extra);
        browser.extras = true;
        assert!(browser.selected().unwrap().extra);
        browser.query = "no match".to_owned();
        browser.refresh(&library, &HashMap::new());
        assert!(browser.selected().is_none());
    }

    #[test]
    fn jumps_between_unwatched_rows_in_the_visible_list() {
        let episodes: Vec<_> = (1..=4)
            .map(|i| crate::library::parse(PathBuf::from(format!("Show - {i:02}.mkv")), None))
            .collect();
        let extra = crate::library::parse(PathBuf::from("Show - 02.5.mkv"), None);
        let mut items = episodes.clone();
        items.insert(2, extra.clone());
        let library = Library {
            groups: [("Show".to_owned(), items)].into(),
            warnings: vec![],
        };
        let watched = Progress {
            watched: true,
            ..Progress::default()
        };
        let history = HashMap::from([
            (episodes[1].path.clone(), watched.clone()),
            (
                episodes[2].path.clone(),
                Progress {
                    position: 60.0,
                    ..Progress::default()
                },
            ),
            (episodes[3].path.clone(), watched),
        ]);
        let mut browser = Browser::new("Show".to_owned());
        browser.refresh(&library, &history);
        browser.episode_state.select(Some(3));

        browser.jump_unwatched(&history, false);
        assert!(browser.episodes_focus);
        assert_eq!(browser.selected().unwrap().path, episodes[0].path);

        browser.jump_unwatched(&history, true);
        assert_eq!(browser.selected().unwrap().path, episodes[0].path);
        assert_eq!(browser.jump_notice, Some("No unwatched videos above"));

        browser.jump_unwatched(&history, false);
        assert_eq!(browser.selected().unwrap().path, episodes[2].path);

        browser.jump_unwatched(&history, false);
        assert_eq!(browser.selected().unwrap().path, episodes[2].path);
        assert_eq!(browser.jump_notice, Some("No unwatched videos below"));

        browser.jump_unwatched(&history, true);
        assert_eq!(browser.selected().unwrap().path, episodes[0].path);

        browser.episodes_focus = false;
        browser.extras = true;
        browser.jump_unwatched(&history, false);
        assert!(browser.episodes_focus);
        assert_eq!(browser.selected().unwrap().path, extra.path);
    }

    #[test]
    fn keeps_library_focus_when_every_video_is_watched() {
        let episode = crate::library::parse(PathBuf::from("Show - 01.mkv"), None);
        let history = HashMap::from([(
            episode.path.clone(),
            Progress {
                watched: true,
                ..Progress::default()
            },
        )]);
        let library = Library {
            groups: [("Show".to_owned(), vec![episode])].into(),
            warnings: vec![],
        };
        let mut browser = Browser::new(String::new());
        browser.refresh(&library, &history);

        browser.jump_unwatched(&history, false);
        assert!(!browser.episodes_focus);
        assert_eq!(browser.episode_state.selected(), Some(0));
        assert_eq!(
            browser.jump_notice,
            Some("No unwatched videos in this list")
        );
    }
}
