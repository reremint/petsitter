//! Application state and the main event loop.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant, SystemTime};

use color_eyre::eyre::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};

use crate::actions;
use crate::background::Refresher;
use crate::model::{Activity, Session};
use crate::monitor::Snapshot;
use crate::pulse::Pulse;
use crate::tree::{self, Row, RowKey};
use crate::ui::Theme;
use crate::view::{self, SessionKey, View};

/// Frame interval while something is animated (spinners).
const ANIMATION_TICK: Duration = Duration::from_millis(40);
/// Frame interval otherwise, enough to keep relative times fresh.
const IDLE_TICK: Duration = Duration::from_secs(1);
/// How long a status message stays in the footer.
const MESSAGE_TTL: Duration = Duration::from_secs(4);

/// The two available screen layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ViewMode {
    /// Machine and quotas on top, sessions in a tree of folders, details of
    /// the selected session or git folder docked at the bottom.
    #[value(alias = "cards")]
    Tree,
    /// A borderless list, one line per session.
    Flat,
}

/// A branch sessions are working on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBranch {
    pub name: String,
    /// Names of the sessions on it.
    pub sessions: Vec<String>,
    /// The folder they run in (the repository, or a linked worktree).
    pub folder: PathBuf,
}

/// How long repository details are kept before they are read again.
const GIT_TTL: Duration = Duration::from_secs(5);

// Independent on/off UI flags; an enum would not make them clearer.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct App {
    pub root: PathBuf,
    pub idle_after: Duration,
    pub theme: Theme,
    pub snapshot: Snapshot,
    pub view: View,
    pub selected: Option<SessionKey>,
    pub show_detail: bool,
    pub mode: ViewMode,
    /// Whether exited sessions are listed.
    pub show_exited: bool,
    /// Whether the tree leaves exited sessions out altogether.
    pub hide_exited: bool,
    pub pulses: HashMap<SessionKey, Pulse>,
    /// Folders the user opened (`true`) or closed (`false`) in the tree.
    pub folds: HashMap<PathBuf, bool>,
    /// Selected row of the tree.
    pub cursor: Option<RowKey>,
    /// Global CPU use over time, oldest first, one sample per second at most.
    pub cpu_history: VecDeque<f32>,
    /// The same, per core.
    pub core_history: Vec<VecDeque<f32>>,
    last_sample: Option<Instant>,
    /// A delete waiting for its confirming second Ctrl+X.
    pending_delete: Option<(SessionKey, Instant)>,
    /// Repository details of folders, refreshed every few seconds.
    git: Refresher<crate::git::GitInfo>,
    /// Branch overviews of folders, with the branches they were made for.
    overviews: Refresher<crate::git::Overview>,
    /// Commits' files and patches (they never change).
    diffs: Refresher<crate::git::CommitDiff>,
    /// The commit whose changes are shown beside the folder panel: its
    /// repository and full hash.
    pub open_commit: Option<(PathBuf, String)>,
    /// How far the open commit's changes are scrolled, in lines.
    pub diff_scroll: usize,
    /// The most the open commit's changes could scroll in the last frame.
    pub diff_most: std::cell::Cell<usize>,
    /// Where the open commit's changes were drawn, for the mouse wheel.
    pub diff_area: std::cell::Cell<ratatui::layout::Rect>,
    /// The tree's scroll set with the mouse wheel; `None` follows the
    /// selection.
    pub tree_scroll: Option<usize>,
    /// The tree's scroll in the last frame, and the most it could scroll.
    pub tree_offset: std::cell::Cell<(usize, usize)>,
    /// Clickable commits of the folder panel in the last frame: screen row
    /// (relative to the panel while it is drawn) and full hash.
    pub commit_hits: RefCell<Vec<(u16, String)>>,
    /// The tree's rows, built once per frame (`None` when stale).
    rows: RefCell<Option<Rc<Vec<Row>>>>,
    /// Clickable rows of the last frame: top line, height and row.
    hits: RefCell<Vec<(u16, u16, RowKey)>>,
    pub message: Option<(String, Instant)>,
    /// Messages from work done on other threads (deletions), shown in turn.
    notices: (
        std::sync::mpsc::Sender<String>,
        std::sync::mpsc::Receiver<String>,
    ),
    /// Time since the start, which drives the animations.
    pub elapsed: Duration,
    started: Instant,
    pub now: SystemTime,
    /// Whether at least one snapshot arrived.
    pub loaded: bool,
    quit: bool,
}

impl App {
    pub fn new(root: PathBuf, idle_after: Duration, theme: Theme, mode: ViewMode) -> Self {
        Self {
            root,
            idle_after,
            theme,
            snapshot: Snapshot::default(),
            view: View::default(),
            selected: None,
            show_detail: true,
            mode,
            show_exited: false,
            hide_exited: true,
            pulses: HashMap::new(),
            folds: HashMap::new(),
            cursor: None,
            hits: RefCell::new(Vec::new()),
            rows: RefCell::new(None),
            git: Refresher::new(GIT_TTL),
            overviews: Refresher::new(GIT_TTL),
            diffs: Refresher::new(Duration::from_secs(3600)),
            open_commit: None,
            diff_scroll: 0,
            diff_area: std::cell::Cell::new(ratatui::layout::Rect::default()),
            diff_most: std::cell::Cell::new(0),
            tree_scroll: None,
            tree_offset: std::cell::Cell::new((0, 0)),
            commit_hits: RefCell::new(Vec::new()),
            pending_delete: None,
            cpu_history: VecDeque::new(),
            core_history: Vec::new(),
            last_sample: None,
            message: None,
            notices: std::sync::mpsc::channel(),
            elapsed: Duration::ZERO,
            started: Instant::now(),
            now: SystemTime::now(),
            loaded: false,
            quit: false,
        }
    }

    pub fn run(
        mut self,
        terminal: &mut DefaultTerminal,
        snapshots: &Receiver<Snapshot>,
    ) -> Result<()> {
        while !self.quit {
            loop {
                match snapshots.try_recv() {
                    Ok(snapshot) => self.apply(snapshot),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        color_eyre::eyre::bail!("the monitor thread stopped unexpectedly")
                    }
                }
            }
            self.now = SystemTime::now();
            while let Ok(notice) = self.notices.1.try_recv() {
                self.notify(notice);
            }
            self.view = View::build(
                &self.snapshot.sessions,
                &self.root,
                self.now,
                self.idle_after,
            );
            if self
                .message
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() > MESSAGE_TTL)
            {
                self.message = None;
            }

            self.elapsed = self.started.elapsed();
            self.rows.take();
            terminal.draw(|frame| crate::ui::render(&self, frame))?;

            let tick = if self.is_animating() {
                ANIMATION_TICK
            } else {
                IDLE_TICK
            };
            if event::poll(tick)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
                    Event::Mouse(mouse) => self.on_mouse(mouse),
                    _ => {}
                }
                self.rows.take();
            }
        }
        Ok(())
    }

    fn apply(&mut self, snapshot: Snapshot) {
        let now = SystemTime::now();
        for session in &snapshot.sessions {
            self.pulses
                .entry(view::key(session))
                .or_default()
                .observe(session, now);
        }
        self.pulses
            .retain(|key, _| snapshot.sessions.iter().any(|s| &view::key(s) == key));
        self.record_cpu(&snapshot.system.machine);
        self.snapshot = snapshot;
        self.loaded = true;
    }

    /// Adds the machine's CPU use to the history graphs.
    fn record_cpu(&mut self, machine: &crate::procs::Machine) {
        const KEEP: usize = 512;
        const SAMPLE_EVERY: Duration = Duration::from_secs(1);
        if machine.cores == 0
            || self
                .last_sample
                .is_some_and(|at| at.elapsed() < SAMPLE_EVERY)
        {
            return;
        }
        self.last_sample = Some(Instant::now());
        let push = |history: &mut VecDeque<f32>, value: f32| {
            history.push_back(value);
            if history.len() > KEEP {
                history.pop_front();
            }
        };
        push(&mut self.cpu_history, machine.cpu);
        self.core_history
            .resize_with(machine.per_core.len(), VecDeque::new);
        for (history, &value) in self.core_history.iter_mut().zip(&machine.per_core) {
            push(history, value);
        }
    }

    /// Installs a snapshot and prepares the view, as the event loop does.
    pub fn load_snapshot(&mut self, snapshot: Snapshot) {
        self.apply(snapshot);
        self.rows.take();
        self.now = SystemTime::now();
        self.view = View::build(
            &self.snapshot.sessions,
            &self.root,
            self.now,
            self.idle_after,
        );
    }

    /// Session indices in on-screen order.
    pub fn order(&self) -> Vec<usize> {
        self.view
            .display_order(&self.snapshot.sessions, self.show_exited)
    }

    fn is_animating(&self) -> bool {
        self.view
            .repos
            .iter()
            .flat_map(|r| &r.sessions)
            .any(|&i| self.snapshot.sessions[i].activity == Activity::Working)
    }

    /// Visible rows of the folder tree, built once per frame.
    pub fn tree_rows(&self) -> Rc<Vec<Row>> {
        if let Some(rows) = self.rows.borrow().as_ref() {
            return Rc::clone(rows);
        }
        let rows = Rc::new(tree::rows(
            &self.snapshot.sessions,
            &self.root,
            self.now,
            self.idle_after,
            &self.folds,
            self.hide_exited,
        ));
        *self.rows.borrow_mut() = Some(Rc::clone(&rows));
        rows
    }

    /// Index of the selected tree row, falling back to the first row.
    pub fn tree_cursor(&self, rows: &[Row]) -> Option<usize> {
        let sessions = &self.snapshot.sessions;
        self.cursor
            .as_ref()
            .and_then(|key| rows.iter().position(|r| &r.key(sessions) == key))
            .or_else(|| (!rows.is_empty()).then_some(0))
    }

    /// The selected session, falling back to the first one on screen.
    pub fn selected_session(&self) -> Option<&Session> {
        let sessions = &self.snapshot.sessions;
        if self.mode == ViewMode::Tree {
            let rows = self.tree_rows();
            return match self.tree_cursor(&rows).map(|i| &rows[i]) {
                Some(Row::Session { index, .. }) => Some(&sessions[*index]),
                _ => None,
            };
        }
        let order = self.order();
        let find = |key: &SessionKey| {
            order
                .iter()
                .copied()
                .find(|&i| &view::key(&sessions[i]) == key)
        };
        self.selected
            .as_ref()
            .and_then(find)
            .or_else(|| order.first().copied())
            .map(|i| &sessions[i])
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Keys move the selection: the tree follows it again.
        self.tree_scroll = None;
        match key.code {
            // Esc closes an open commit first.
            KeyCode::Esc if self.open_commit.is_some() => self.open_commit = None,
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::PageDown if self.open_commit.is_some() => {
                self.diff_scroll = (self.diff_scroll + 10).min(self.diff_most.get());
            }
            KeyCode::PageUp if self.open_commit.is_some() => {
                self.diff_scroll = self.diff_scroll.saturating_sub(10);
            }
            KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('x') if ctrl => self.delete_selected(),
            KeyCode::Char('y') => self.copy_resume(),
            KeyCode::Char('d') => self.show_detail = !self.show_detail,
            KeyCode::Char('h') if self.mode == ViewMode::Tree => {
                self.hide_exited = !self.hide_exited;
            }
            _ if self.mode == ViewMode::Tree => self.on_tree_key(key.code),
            KeyCode::Char('j') | KeyCode::Tab | KeyCode::Right | KeyCode::Down => {
                self.move_selection(1);
            }
            KeyCode::Char('k') | KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                self.move_selection(-1);
            }
            KeyCode::Home | KeyCode::Char('g') => self.move_selection(isize::MIN),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX),
            KeyCode::Char('h') => self.show_exited = !self.show_exited,
            KeyCode::Enter => self.focus_selected(),
            _ => {}
        }
    }

    /// The repository details of `dir`, read in the background and kept for
    /// a few seconds.
    pub fn git_info(&self, dir: &std::path::Path) -> Option<Arc<crate::git::GitInfo>> {
        let owned = dir.to_path_buf();
        self.git
            .get(dir, Vec::new(), move || crate::git::info(&owned))
    }

    /// The branches sessions are working on in the repository at `dir`
    /// (sessions in its linked worktrees included): each branch, the names
    /// of those sessions, and the folder they run in. Exited sessions are
    /// left out.
    pub fn live_branches(&self, dir: &std::path::Path) -> Vec<LiveBranch> {
        let mut branches: Vec<LiveBranch> = Vec::new();
        for session in &self.snapshot.sessions {
            if crate::tree::Status::of(session, self.now, self.idle_after)
                == crate::tree::Status::Exited
            {
                continue;
            }
            let in_repo = session.worktree.as_ref().is_some_and(|w| w.main == dir)
                || session.cwd.starts_with(dir);
            let branch = session
                .worktree
                .as_ref()
                .and_then(|w| w.branch.clone())
                .or_else(|| session.git_branch.clone())
                .filter(|b| b != "HEAD");
            let (true, Some(branch)) = (in_repo, branch) else {
                continue;
            };
            match branches.iter_mut().find(|b| b.name == branch) {
                Some(found) => found.sessions.push(session.name.clone()),
                None => branches.push(LiveBranch {
                    name: branch,
                    sessions: vec![session.name.clone()],
                    folder: session.cwd.clone(),
                }),
            }
        }
        branches.sort_by(|a, b| a.name.cmp(&b.name));
        branches
    }

    /// The branch overview of the repository at `dir`, for the branches its
    /// sessions work on; refreshed every few seconds.
    pub fn overview(&self, dir: &std::path::Path) -> Option<Arc<crate::git::Overview>> {
        let branches: Vec<String> = self
            .live_branches(dir)
            .into_iter()
            .map(|b| b.name)
            .collect();
        let owned = dir.to_path_buf();
        let wanted = branches.clone();
        self.overviews
            .get(dir, branches, move || crate::git::overview(&owned, &wanted))
    }

    /// The mouse wheel scrolls what is under the pointer, leaving the
    /// selection where it is: an open commit's changes, or the tree (the
    /// flat list still moves its selection).
    fn on_wheel(&mut self, mouse: MouseEvent, lines: isize) {
        let over_diff = self.open_commit.is_some()
            && self
                .diff_area
                .get()
                .contains(ratatui::layout::Position::new(mouse.column, mouse.row));
        if over_diff {
            self.diff_scroll = self
                .diff_scroll
                .saturating_add_signed(lines)
                .min(self.diff_most.get());
        } else if self.mode == ViewMode::Tree {
            let (offset, most) = self.tree_offset.get();
            self.tree_scroll = Some(offset.saturating_add_signed(lines).min(most));
        } else {
            let code = if lines > 0 {
                KeyCode::Down
            } else {
                KeyCode::Up
            };
            self.on_key(KeyEvent::from(code));
        }
    }

    /// The commit shown beside the selected folder's panel, if any.
    pub fn shown_commit(&self) -> Option<String> {
        let (dir, hash) = self.open_commit.as_ref()?;
        (self.selected_folder().as_ref() == Some(dir)).then(|| hash.clone())
    }

    /// The files and patch of commit `hash` in `dir`, read in the background.
    pub fn commit_diff(
        &self,
        dir: &std::path::Path,
        hash: &str,
    ) -> Option<Arc<crate::git::CommitDiff>> {
        let owned = dir.to_path_buf();
        let wanted = hash.to_owned();
        self.diffs.get(&dir.join(hash), Vec::new(), move || {
            crate::git::commit_diff(&owned, &wanted)
        })
    }

    /// The folder under the tree cursor, if any.
    pub fn selected_folder(&self) -> Option<PathBuf> {
        if self.mode != ViewMode::Tree {
            return None;
        }
        let rows = self.tree_rows();
        match self.tree_cursor(&rows).map(|i| &rows[i]) {
            Some(Row::Folder { path, .. }) => Some(path.clone()),
            _ => None,
        }
    }

    /// Records the clickable rows of the frame being drawn.
    pub fn set_hits(&self, hits: impl IntoIterator<Item = (u16, u16, RowKey)>) {
        *self.hits.borrow_mut() = hits.into_iter().collect();
    }

    /// Mouse: a click selects a row (and opens or closes a folder, or jumps to
    /// an already selected session), or opens a commit in a folder's panel;
    /// the wheel scrolls (see [`Self::on_wheel`]).
    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.on_wheel(mouse, 3),
            MouseEventKind::ScrollUp => self.on_wheel(mouse, -3),
            MouseEventKind::Down(MouseButton::Left) => {
                let commit = self
                    .commit_hits
                    .borrow()
                    .iter()
                    .find(|(row, _)| *row == mouse.row)
                    .map(|(_, hash)| hash.clone());
                if let (Some(hash), Some(folder)) = (commit, self.selected_folder()) {
                    // A second click on the open commit closes it.
                    let open = Some((folder.clone(), hash.clone()));
                    self.open_commit = if self.open_commit == open { None } else { open };
                    self.diff_scroll = 0;
                    return;
                }
                let hit = self
                    .hits
                    .borrow()
                    .iter()
                    .find(|(top, height, _)| (*top..top + height).contains(&mouse.row))
                    .map(|(.., key)| key.clone());
                let Some(key) = hit else { return };
                match &key {
                    RowKey::Folder(path) => {
                        let rows = self.tree_rows();
                        let open = rows.iter().any(
                            |r| matches!(r, Row::Folder { path: p, open: true, .. } if p == path),
                        );
                        self.folds.insert(path.clone(), !open);
                        self.cursor = Some(key);
                    }
                    RowKey::More(path) => {
                        self.folds.insert(path.clone(), true);
                        self.cursor = Some(RowKey::Folder(path.clone()));
                    }
                    RowKey::Session(session) => {
                        let already = match self.mode {
                            ViewMode::Tree => self.cursor.as_ref() == Some(&key),
                            ViewMode::Flat => {
                                self.selected_session().map(view::key).as_ref() == Some(session)
                            }
                        };
                        self.cursor = Some(key.clone());
                        self.selected = Some(session.clone());
                        if already {
                            self.focus_selected();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Tree navigation: ↑↓ move, → opens a folder, ← closes it or goes up to
    /// the parent folder, space toggles, ⏎ toggles folders and jumps to agents.
    fn on_tree_key(&mut self, code: KeyCode) {
        let rows = self.tree_rows();
        let Some(current) = self.tree_cursor(&rows) else {
            return;
        };
        let sessions = &self.snapshot.sessions;
        let last = rows.len() - 1;
        let target = match code {
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => Some((current + 1).min(last)),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => Some(current.saturating_sub(1)),
            KeyCode::Home | KeyCode::Char('g') => Some(0),
            KeyCode::End | KeyCode::Char('G') => Some(last),
            _ => None,
        };
        if let Some(target) = target {
            self.cursor = Some(rows[target].key(sessions));
            return;
        }
        match (&rows[current], code) {
            (Row::Folder { path, open, .. }, KeyCode::Char(' ') | KeyCode::Enter) => {
                self.folds.insert(path.clone(), !open);
            }
            (
                Row::Folder {
                    path, open: false, ..
                },
                KeyCode::Right | KeyCode::Char('l'),
            ) => {
                self.folds.insert(path.clone(), true);
            }
            (
                Row::Folder {
                    path, open: true, ..
                },
                KeyCode::Left,
            ) => {
                self.folds.insert(path.clone(), false);
            }
            (Row::More { path, .. }, KeyCode::Char(' ') | KeyCode::Enter | KeyCode::Right) => {
                let path = path.clone();
                self.folds.insert(path.clone(), true);
                self.cursor = Some(RowKey::Folder(path));
            }
            (row, KeyCode::Left) => {
                // Jump to the enclosing folder: the nearest shallower row above.
                let depth = row.depth();
                if let Some(parent) = rows[..current]
                    .iter()
                    .rev()
                    .find(|r| matches!(r, Row::Folder { depth: d, .. } if *d < depth))
                {
                    self.cursor = Some(parent.key(sessions));
                }
            }
            (Row::Session { .. }, KeyCode::Enter) => self.focus_selected(),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let order = self.order();
        if order.is_empty() {
            return;
        }
        let sessions = &self.snapshot.sessions;
        let current = self
            .selected_session()
            .and_then(|s| {
                order
                    .iter()
                    .position(|&i| sessions[i].id == s.id && sessions[i].agent == s.agent)
            })
            .unwrap_or(0);
        let last = order.len() - 1;
        let target = match delta {
            isize::MIN => 0,
            isize::MAX => last,
            d => current.saturating_add_signed(d).min(last),
        };
        self.selected = Some(view::key(&sessions[order[target]]));
    }

    fn focus_selected(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let result = match (&session.activity, &session.rex) {
            (Activity::Exited, _) => {
                Err("session exited — press y to copy its resume command".to_owned())
            }
            (_, None) => Err("no Rex pane known for this session".to_owned()),
            (_, Some(target)) => actions::focus_rex(target).map_err(|e| e.to_string()),
        };
        if let Err(message) = result {
            self.notify(message);
        }
    }

    /// Ctrl+X on an exited session asks for confirmation; a second Ctrl+X
    /// within a few seconds deletes its conversation.
    fn delete_selected(&mut self) {
        const CONFIRM_WINDOW: Duration = Duration::from_secs(4);
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        if session.activity != Activity::Exited {
            self.notify("only exited sessions can be deleted".to_owned());
            return;
        }
        let key = view::key(&session);
        let confirmed = self
            .pending_delete
            .take()
            .is_some_and(|(pending, at)| pending == key && at.elapsed() < CONFIRM_WINDOW);
        if !confirmed {
            self.pending_delete = Some((key, Instant::now()));
            self.notify(format!("press ctrl+x again to delete {}", session.name));
            return;
        }
        // Agents' CLIs can take a while: delete on a thread, and drop the
        // session from the list now rather than at the next refresh.
        self.snapshot.sessions.retain(|s| view::key(s) != key);
        self.notify(format!("deleting {}…", session.name));
        let notices = self.notices.0.clone();
        std::thread::spawn(move || {
            let message = match actions::delete_session(&session) {
                Ok(()) => format!("deleted {}", session.name),
                Err(e) => format!("delete failed: {e}"),
            };
            let _ = notices.send(message);
        });
    }

    fn copy_resume(&mut self) {
        let Some(session) = self.selected_session() else {
            return;
        };
        let command = session.resume_command();
        let message = match actions::copy_to_clipboard(&command) {
            Ok(()) => format!("copied: {command}"),
            Err(e) => format!("copy failed: {e}"),
        };
        self.notify(message);
    }

    fn notify(&mut self, message: String) {
        self.message = Some((message, Instant::now()));
    }
}
