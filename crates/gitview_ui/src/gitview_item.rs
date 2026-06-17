use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Task, WeakEntity, Window,
};
use lean_git_core::AppError;
use lean_git_core::models::{
    CommitSummary, DiffLineKind, DiffResult, FileStatus, HistoryPage, RepoStatus, StatusBucket,
};
use lean_git_core::service::AppService;

use crate::graph::{self, GraphLayout, GraphMetrics};
use project::Project;
use settings::Settings as _;
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use ui::{Checkbox, Divider, LabelSize, ToggleState, Tooltip, prelude::*};
use ui_input::InputField;
use workspace::Item;
use workspace::Workspace;

/// Maximum number of diff rows (hunk headers + lines) rendered at once. The
/// engine already caps diff bytes, but a single large file can still produce
/// thousands of lines; we cap the rendered element count to keep the view
/// responsive.
const MAX_DIFF_LINES: usize = 3000;

/// The unique name passed to the engine's config registry. The engine persists a
/// small repo registry under this app name; it is shared across GitView tabs.
const APP_NAME: &str = "zed-gitview";

pub enum GitViewEvent {}

/// Whether the repository has at least one staged change ready to commit.
fn has_staged_changes(status: Option<&RepoStatus>) -> bool {
    status.is_some_and(|status| !status.staged.is_empty())
}

/// Returns why a commit is currently blocked, or `None` if it is allowed: the
/// repo must be trusted, have at least one staged change, and a non-blank
/// message. The returned string is shown to the user.
fn commit_block_reason(
    trusted: bool,
    status: Option<&RepoStatus>,
    message: &str,
) -> Option<&'static str> {
    if !trusted {
        Some("Repository is not trusted; staging and commits are disabled.")
    } else if !has_staged_changes(status) {
        Some("No staged changes to commit.")
    } else if message.trim().is_empty() {
        Some("Enter a commit message before committing.")
    } else {
        None
    }
}

/// Looks up which bucket a path currently lives in, returning `Some(true)` if it
/// is staged, `Some(false)` if it is unstaged/untracked/conflicted, or `None` if
/// the path is no longer present in the status. Used to keep the selected diff in
/// sync after a stage/unstage/commit changes which side a file lives on.
fn bucket_staged_flag(status: &RepoStatus, path: &str) -> Option<bool> {
    if status.staged.iter().any(|file| file.path == path) {
        Some(true)
    } else if status.unstaged.iter().any(|file| file.path == path)
        || status.untracked.iter().any(|file| file.path == path)
        || status.conflicted.iter().any(|file| file.path == path)
    {
        Some(false)
    } else {
        None
    }
}

/// The single-character gutter prefix shown for a diff line of the given kind.
fn diff_line_prefix(kind: DiffLineKind) -> &'static str {
    match kind {
        DiffLineKind::Added => "+",
        DiffLineKind::Removed => "-",
        DiffLineKind::Context => " ",
        DiffLineKind::Hunk | DiffLineKind::Meta => "",
    }
}

/// Which workspace view the GitView tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    /// Working-tree status: stage/unstage, commit, and per-file diff.
    Changes,
    /// Commit history rendered as a branch graph.
    History,
}

/// How many commits to request for the history graph in one page.
const HISTORY_PAGE_LIMIT: usize = 300;

/// Collects every path eligible for a "Stage all": all unstaged and untracked
/// entries (conflicted files are excluded — they need resolution first). The
/// result is de-duplicated because a path can appear in more than one bucket.
fn stageable_paths(status: &RepoStatus) -> Vec<String> {
    let mut paths = Vec::new();
    for file in status.unstaged.iter().chain(status.untracked.iter()) {
        if !paths.contains(&file.path) {
            paths.push(file.path.clone());
        }
    }
    paths
}

/// Collects every staged path for an "Unstage all".
fn unstageable_paths(status: &RepoStatus) -> Vec<String> {
    let mut paths = Vec::new();
    for file in &status.staged {
        if !paths.contains(&file.path) {
            paths.push(file.path.clone());
        }
    }
    paths
}

/// A short, human-friendly age like `5m`, `3h`, `2d`, `4w`, `1y` from a unix
/// timestamp (seconds) to now. Used for the commit list's right-aligned age.
fn relative_age(author_time: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(author_time);
    let delta = (now - author_time).max(0);
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;
    const YEAR: i64 = 365 * DAY;
    if delta < MINUTE {
        "now".to_string()
    } else if delta < HOUR {
        format!("{}m", delta / MINUTE)
    } else if delta < DAY {
        format!("{}h", delta / HOUR)
    } else if delta < WEEK {
        format!("{}d", delta / DAY)
    } else if delta < YEAR {
        format!("{}w", delta / WEEK)
    } else {
        format!("{}y", delta / YEAR)
    }
}

/// Trims a decoration ref (e.g. `HEAD -> main`, `origin/main`, `tag: v1`) to a
/// compact badge label.
fn ref_badge_label(reference: &str) -> String {
    let reference = reference.trim();
    if let Some(head) = reference.strip_prefix("HEAD -> ") {
        head.to_string()
    } else if let Some(tag) = reference.strip_prefix("tag: ") {
        tag.to_string()
    } else {
        reference.to_string()
    }
}

/// A center-pane workspace item that renders GitView's git workflow (status,
/// stage/unstage, commit, diff) for a single repository, backed by the
/// `lean_git_core` engine.
///
/// The repository is resolved once, when the tab is opened, from the workspace's
/// active repository; the tab stays pinned to that repository for its lifetime.
pub struct GitViewItem {
    /// The engine handle. `None` until the background open pipeline has loaded it
    /// (loading touches the filesystem, so it does not run on the UI thread).
    service: Option<AppService>,
    repo_id: String,
    repo_root: Option<Arc<Path>>,
    status: Option<RepoStatus>,
    /// The selected file and the side (`true` = staged) its diff was fetched for.
    selected: Option<(String, bool)>,
    diff: Option<DiffResult>,
    commit_input: Entity<InputField>,
    trusted: bool,
    loading: bool,
    /// True while a network op (fetch/pull/push) is in flight, so the remote
    /// buttons disable and the top bar shows progress.
    remote_busy: bool,
    error: Option<String>,
    /// Which view (Changes / History) is active.
    view: ViewMode,
    /// Loaded commit history for the graph view; `None` until first opened.
    history: Option<HistoryPage>,
    /// Pre-computed branch-graph routing for `history`, shared into the painter.
    graph_layout: Option<Arc<GraphLayout>>,
    /// True while the history page is being fetched.
    history_loading: bool,
    /// The commit whose diff is currently shown in the history view.
    selected_commit: Option<String>,
    focus_handle: FocusHandle,
    /// Tracks the in-flight open/refresh task. Superseding it is safe: the latest
    /// status read wins.
    _status_task: Task<()>,
    /// Tracks the in-flight diff fetch. Superseding it is safe: only the latest
    /// selection's diff matters.
    _diff_task: Task<()>,
    /// Tracks the in-flight history fetch. Superseding it is safe: the latest
    /// page wins.
    _history_task: Task<()>,
}

impl GitViewItem {
    /// Opens (or focuses an existing) GitView tab in the active pane for the
    /// repository currently open in the workspace.
    pub fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let pane = workspace.active_pane().clone();
        let existing = pane
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<GitViewItem>());
        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
            return;
        }

        let project = workspace.project().clone();
        let active_repository = project.read(cx).active_repository(cx);
        let (repo_root, repo_trusted) = active_repository
            .as_ref()
            .map(|repo| {
                let repo = repo.read(cx);
                (
                    Some(repo.work_directory_abs_path.clone()),
                    repo.is_trusted(),
                )
            })
            .unwrap_or((None, false));
        let workspace_handle = cx.weak_entity();

        let item = cx.new(|cx| {
            GitViewItem::new(
                repo_root,
                repo_trusted,
                project,
                workspace_handle,
                window,
                cx,
            )
        });
        workspace.add_item_to_active_pane(Box::new(item), None, true, window, cx);
    }

    fn new(
        repo_root: Option<Arc<Path>>,
        repo_trusted: bool,
        _project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let commit_input = cx.new(|cx| InputField::new(window, cx, "Commit message"));

        let mut this = Self {
            service: None,
            repo_id: String::new(),
            repo_root: repo_root.clone(),
            status: None,
            selected: None,
            diff: None,
            commit_input,
            trusted: repo_trusted,
            loading: false,
            remote_busy: false,
            error: None,
            view: ViewMode::Changes,
            history: None,
            graph_layout: None,
            history_loading: false,
            selected_commit: None,
            focus_handle,
            _status_task: Task::ready(()),
            _diff_task: Task::ready(()),
            _history_task: Task::ready(()),
        };

        match repo_root {
            Some(root) => this.start_open(root, repo_trusted, cx),
            None => {
                this.error = Some("No git repository is open in this project.".into());
            }
        }

        this
    }

    /// Loads the engine, registers the open repo, applies Zed's trust decision,
    /// and reads its status. All of this runs on a background thread because
    /// every step is blocking I/O (config load, git executable resolution, and
    /// `git` calls).
    fn start_open(&mut self, root: Arc<Path>, repo_trusted: bool, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        self.trusted = repo_trusted;
        self._status_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    let service = AppService::load(APP_NAME);
                    let summary = service.add_repo(root.as_ref())?;
                    let id = summary.id.clone();
                    service.set_active_repo(&id)?;
                    if repo_trusted {
                        service.set_repo_trusted_by_host(&id, true)?;
                    } else {
                        service.set_repo_trusted_by_host(&id, false)?;
                        let _ = service.set_repo_trusted(&id, false);
                    }
                    let status = service.refresh_repo(id.clone())?;
                    Ok::<_, AppError>((service, id, status))
                })
                .await;

            this.update(cx, |this, cx| {
                this.loading = false;
                match outcome {
                    Ok((service, id, status)) => {
                        this.service = Some(service);
                        this.repo_id = id;
                        this.status = Some(status);
                        this.trusted = repo_trusted;
                        this.error = if repo_trusted {
                            None
                        } else {
                            Some(
                                "Repository is not trusted in Zed; staging and commits are disabled."
                                    .into(),
                            )
                        };
                    }
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            // The engine never loaded (e.g. the open failed); retry the full
            // open pipeline if we still have a repo root.
            if let Some(root) = self.repo_root.clone() {
                self.start_open(root, self.trusted, cx);
            }
            return;
        };
        if self.repo_id.is_empty() {
            return;
        }
        let id = self.repo_id.clone();
        self._status_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { service.refresh_repo(id) })
                .await;
            this.update(cx, |this, cx| {
                this.apply_status_outcome(outcome);
                this.reload_selected_diff(cx);
                cx.notify();
            })
            .ok();
        });
    }

    fn set_staged(&mut self, path: String, stage: bool, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() {
            return;
        }
        if !self.trusted {
            self.error = Some("Repository is not trusted; staging is disabled.".into());
            cx.notify();
            return;
        }
        let id = self.repo_id.clone();
        let paths = vec![path];
        // Mutations are detached rather than stored in a cancellable slot so a
        // later action (another stage click, a refresh, selecting a file) can
        // never silently cancel an in-flight stage/unstage. The closure uses a
        // weak handle, so a closed tab just drops the UI update.
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    if stage {
                        service.stage_paths(id, paths)
                    } else {
                        service.unstage_paths(id, paths)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_status_outcome(outcome);
                this.reload_selected_diff(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stages or unstages many paths in one engine call. Used by Stage all /
    /// Unstage all. Detached for the same reason as `set_staged`.
    fn set_staged_many(&mut self, paths: Vec<String>, stage: bool, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() || paths.is_empty() {
            return;
        }
        if !self.trusted {
            self.error = Some("Repository is not trusted; staging is disabled.".into());
            cx.notify();
            return;
        }
        let id = self.repo_id.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    if stage {
                        service.stage_paths(id, paths)
                    } else {
                        service.unstage_paths(id, paths)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.apply_status_outcome(outcome);
                this.reload_selected_diff(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn stage_all(&mut self, cx: &mut Context<Self>) {
        let Some(paths) = self.status.as_ref().map(stageable_paths) else {
            return;
        };
        self.set_staged_many(paths, true, cx);
    }

    fn unstage_all(&mut self, cx: &mut Context<Self>) {
        let Some(paths) = self.status.as_ref().map(unstageable_paths) else {
            return;
        };
        self.set_staged_many(paths, false, cx);
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        let message = self.commit_input.read(cx).text(cx).trim().to_string();
        if self.repo_id.is_empty() {
            return;
        }
        if let Some(reason) = commit_block_reason(self.trusted, self.status.as_ref(), &message) {
            self.error = Some(reason.to_string());
            cx.notify();
            return;
        }
        let id = self.repo_id.clone();
        self.commit_input
            .update(cx, |input, cx| input.clear(window, cx));
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { service.commit(id, message) })
                .await;
            this.update(cx, |this, cx| {
                this.apply_status_outcome(outcome);
                this.reload_selected_diff(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Runs a network git op (fetch/pull/push) off the UI thread and applies the
    /// resulting status. Detached (not stored in a cancellable slot) so a later
    /// refresh or selection can never silently cancel an in-flight network op.
    fn run_remote_op(
        &mut self,
        op: impl FnOnce(AppService, String) -> Result<RepoStatus, AppError> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() || self.remote_busy {
            return;
        }
        let id = self.repo_id.clone();
        self.remote_busy = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { op(service, id) }).await;
            this.update(cx, |this, cx| {
                this.remote_busy = false;
                this.apply_status_outcome(outcome);
                this.reload_selected_diff(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn fetch(&mut self, cx: &mut Context<Self>) {
        self.run_remote_op(|service, id| service.fetch_repo(id), cx);
    }

    fn pull(&mut self, cx: &mut Context<Self>) {
        self.run_remote_op(|service, id| service.pull_ff_only(id), cx);
    }

    fn push(&mut self, cx: &mut Context<Self>) {
        // `None` remote_url pushes to the branch's configured upstream; the
        // engine fails closed if the repo is untrusted or has no upstream.
        self.run_remote_op(|service, id| service.push_repo(id, None), cx);
    }

    fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        // Switching panes drops the cross-pane diff so we don't show a file diff
        // under the history list or vice versa.
        self.diff = None;
        if view == ViewMode::History && self.history.is_none() {
            self.load_history(cx);
        }
        cx.notify();
    }

    /// Loads a page of commit history and pre-computes its branch-graph routing
    /// off the UI thread.
    fn load_history(&mut self, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() {
            return;
        }
        let id = self.repo_id.clone();
        self.history_loading = true;
        self.error = None;
        cx.notify();
        self._history_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move {
                    service
                        .get_history(id, None, Some(HISTORY_PAGE_LIMIT))
                        .map(|page| {
                            let layout = graph::compute_graph_layout(&page.commits);
                            (page, layout)
                        })
                })
                .await;
            this.update(cx, |this, cx| {
                this.history_loading = false;
                match outcome {
                    Ok((page, layout)) => {
                        this.history = Some(page);
                        this.graph_layout = Some(Arc::new(layout));
                    }
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Loads and shows the diff for a single historical commit.
    fn select_commit(&mut self, commit: String, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() {
            return;
        }
        // A commit selection and a working-tree file selection are mutually
        // exclusive in the diff pane.
        self.selected = None;
        self.selected_commit = Some(commit.clone());
        self.diff = None;
        let id = self.repo_id.clone();
        self._diff_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { service.get_commit_diff(id, commit, None) })
                .await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(diff) => this.diff = Some(diff),
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            })
            .ok();
        });
    }

    fn select_file(&mut self, path: String, staged: bool, cx: &mut Context<Self>) {
        let Some(service) = self.service.clone() else {
            return;
        };
        if self.repo_id.is_empty() {
            return;
        }
        self.selected = Some((path.clone(), staged));
        self.selected_commit = None;
        self.diff = None;
        let id = self.repo_id.clone();
        self._diff_task = cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { service.get_diff(id, path, staged) })
                .await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(diff) => this.diff = Some(diff),
                    Err(error) => this.error = Some(error.message),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// After a status change, re-sync the selected file's diff: a file may have
    /// moved between the staged and unstaged sides, or disappeared entirely
    /// (e.g. after a commit). Without this the diff pane shows stale content for
    /// what is about to be committed.
    fn reload_selected_diff(&mut self, cx: &mut Context<Self>) {
        let Some((path, _)) = self.selected.clone() else {
            return;
        };
        match self
            .status
            .as_ref()
            .and_then(|status| bucket_staged_flag(status, &path))
        {
            Some(staged) => self.select_file(path, staged, cx),
            None => {
                self.selected = None;
                self.diff = None;
                cx.notify();
            }
        }
    }

    fn apply_status_outcome(&mut self, outcome: Result<RepoStatus, AppError>) {
        match outcome {
            Ok(status) => {
                self.status = Some(status);
                self.error = None;
            }
            Err(error) => self.error = Some(error.message),
        }
    }

    fn render_top_bar(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status = self.status.as_ref();
        let branch = status
            .and_then(|status| status.branch.clone())
            .unwrap_or_else(|| "(detached)".into());
        let ahead = status.map_or(0, |status| status.ahead);
        let behind = status.map_or(0, |status| status.behind);
        // Remote ops need a loaded repo and no in-flight network op; push also
        // requires the repo to be trusted (the engine enforces this too).
        let repo_ready = self.service.is_some() && !self.repo_id.is_empty();
        let remote_disabled = !repo_ready || self.remote_busy;
        let push_disabled = remote_disabled || !self.trusted;

        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Button::new("gitview-tab-changes", "Changes")
                                    .style(if matches!(self.view, ViewMode::Changes) {
                                        ButtonStyle::Filled
                                    } else {
                                        ButtonStyle::Subtle
                                    })
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.set_view(ViewMode::Changes, cx)
                                    })),
                            )
                            .child(
                                Button::new("gitview-tab-history", "History")
                                    .style(if matches!(self.view, ViewMode::History) {
                                        ButtonStyle::Filled
                                    } else {
                                        ButtonStyle::Subtle
                                    })
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.set_view(ViewMode::History, cx)
                                    })),
                            ),
                    )
                    .child(Icon::new(IconName::GitBranch).size(IconSize::Small))
                    .child(Label::new(branch).weight(gpui::FontWeight::MEDIUM))
                    .when(ahead > 0 || behind > 0, |this| {
                        this.child(
                            Label::new(format!("↑{ahead} ↓{behind}"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(self.loading || self.remote_busy, |this| {
                        this.child(
                            Label::new(if self.remote_busy {
                                "Working…"
                            } else {
                                "Loading…"
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        )
                    })
                    .when(!self.trusted && !self.repo_id.is_empty(), |this| {
                        this.child(
                            Label::new("untrusted")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        IconButton::new("gitview-fetch", IconName::CloudDownload)
                            .icon_size(IconSize::Small)
                            .disabled(remote_disabled)
                            .tooltip(Tooltip::text("Fetch"))
                            .on_click(cx.listener(|this, _, _window, cx| this.fetch(cx))),
                    )
                    .child(
                        IconButton::new("gitview-pull", IconName::ArrowDown)
                            .icon_size(IconSize::Small)
                            .disabled(remote_disabled)
                            .tooltip(Tooltip::text("Pull (fast-forward only)"))
                            .on_click(cx.listener(|this, _, _window, cx| this.pull(cx))),
                    )
                    .child(
                        IconButton::new("gitview-push", IconName::ArrowUp)
                            .icon_size(IconSize::Small)
                            .disabled(push_disabled)
                            .tooltip(Tooltip::text("Push"))
                            .on_click(cx.listener(|this, _, _window, cx| this.push(cx))),
                    )
                    .child(
                        IconButton::new("gitview-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .disabled(self.remote_busy)
                            .tooltip(Tooltip::text("Refresh"))
                            .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
                    ),
            )
    }

    fn render_section(
        &self,
        title: &'static str,
        files: &[FileStatus],
        bucket: StatusBucket,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if files.is_empty() {
            return None;
        }
        let colors = cx.theme().colors();
        let is_staged = matches!(bucket, StatusBucket::Staged);
        let is_conflicted = matches!(bucket, StatusBucket::Conflicted);
        let selected_path = self.selected.as_ref().map(|(path, _)| path.clone());

        let rows = files.iter().map(|file| {
            let path = file.path.clone();
            let selected = selected_path.as_deref() == Some(path.as_str());
            let toggle_state = if is_staged {
                ToggleState::Selected
            } else {
                ToggleState::Unselected
            };
            let checkbox_id = SharedString::from(format!("chk-{title}-{path}"));
            let row_id = SharedString::from(format!("row-{title}-{path}"));

            let stage_path = path.clone();
            let select_path = path.clone();

            h_flex()
                .id(row_id)
                .w_full()
                .items_center()
                .gap_2()
                .px_1()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .when(selected, |this| this.bg(colors.element_selected))
                .hover(|this| this.bg(colors.element_hover))
                // The whole row selects the file (shows its diff); only the
                // checkbox stages/unstages.
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    this.select_file(select_path.clone(), is_staged, cx);
                }))
                .child(
                    Checkbox::new(checkbox_id, toggle_state).on_click(cx.listener(
                        move |this, _state: &ToggleState, _window, cx| {
                            this.set_staged(stage_path.clone(), !is_staged, cx);
                        },
                    )),
                )
                .child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        .child(Label::new(path.clone()).size(LabelSize::Small)),
                )
                .when(is_conflicted, |this| {
                    this.child(
                        Label::new("conflict")
                            .size(LabelSize::Small)
                            .color(Color::Error),
                    )
                })
                .into_any_element()
        });

        Some(
            v_flex()
                .w_full()
                .gap_0p5()
                .child(
                    Label::new(format!("{title} ({})", files.len()))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let has_status = self.status.is_some();
        let sections = self.status.as_ref().map(|status| {
            [
                self.render_section(
                    "Conflicted",
                    &status.conflicted,
                    StatusBucket::Conflicted,
                    cx,
                ),
                self.render_section("Staged", &status.staged, StatusBucket::Staged, cx),
                self.render_section("Unstaged", &status.unstaged, StatusBucket::Unstaged, cx),
                self.render_section("Untracked", &status.untracked, StatusBucket::Untracked, cx),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
        });

        let working_tree_clean = sections
            .as_ref()
            .is_some_and(|sections| sections.is_empty());
        let commit_disabled = !(self.trusted && has_staged_changes(self.status.as_ref()));
        let can_stage_all = self.trusted
            && self
                .status
                .as_ref()
                .is_some_and(|status| !stageable_paths(status).is_empty());
        let can_unstage_all = self.trusted
            && self
                .status
                .as_ref()
                .is_some_and(|status| !status.staged.is_empty());

        let colors = cx.theme().colors();
        v_flex()
            .id("gitview-sidebar")
            .h_full()
            .w(px(320.))
            .flex_none()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        Label::new("Changes")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("gitview-stage-all", "Stage all")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(!can_stage_all)
                                    .on_click(
                                        cx.listener(|this, _event, _window, cx| this.stage_all(cx)),
                                    ),
                            )
                            .child(
                                Button::new("gitview-unstage-all", "Unstage all")
                                    .style(ButtonStyle::Subtle)
                                    .disabled(!can_unstage_all)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.unstage_all(cx)
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("gitview-status-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_2()
                    .when_some(sections, |this, sections| this.children(sections))
                    .when(working_tree_clean, |this| {
                        this.child(
                            Label::new("Working tree clean")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(!has_status && self.error.is_none(), |this| {
                        this.child(
                            Label::new("Loading…")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(Divider::horizontal())
            .child(
                v_flex()
                    .flex_none()
                    .p_2()
                    .gap_2()
                    .child(self.commit_input.clone())
                    .child(
                        Button::new("gitview-commit", "Commit")
                            .full_width()
                            .style(ButtonStyle::Filled)
                            .disabled(commit_disabled)
                            .on_click(
                                cx.listener(|this, _event, window, cx| this.commit(window, cx)),
                            ),
                    ),
            )
    }

    fn render_diff(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let status_colors = cx.theme().status();
        let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();

        let header = if let Some((path, staged)) = self.selected.as_ref() {
            format!("{path}{}", if *staged { "  (staged)" } else { "" })
        } else if let Some(commit) = self.selected_commit.as_ref() {
            let short: String = commit.chars().take(8).collect();
            format!("commit {short}")
        } else if matches!(self.view, ViewMode::History) {
            "Select a commit to view its diff".to_string()
        } else {
            "Select a file to view its diff".to_string()
        };

        // Monospace font + horizontal scroll so diff gutters and indentation
        // line up and long lines don't wrap. `items_start` lets each line size to
        // its content width so the body scrolls horizontally instead of clipping.
        let mut body = v_flex()
            .id("gitview-diff-body")
            .flex_1()
            .items_start()
            .overflow_x_scroll()
            .overflow_y_scroll()
            .p_2()
            .font_family(buffer_font)
            .text_size(px(12.));

        if let Some(diff) = &self.diff {
            if diff.files.is_empty() {
                body = body.child(
                    Label::new("No changes for this file.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                );
            } else {
                let mut rendered = 0usize;
                let mut truncated = false;
                'files: for file in &diff.files {
                    if file.binary {
                        body = body.child(
                            Label::new("Binary file")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                        continue;
                    }
                    for hunk in &file.hunks {
                        if rendered >= MAX_DIFF_LINES {
                            truncated = true;
                            break 'files;
                        }
                        rendered += 1;
                        body = body.child(
                            div()
                                .px_1()
                                .whitespace_nowrap()
                                .text_color(colors.text_accent)
                                .bg(colors.element_background)
                                .child(SharedString::from(hunk.header.clone())),
                        );
                        for line in &hunk.lines {
                            if rendered >= MAX_DIFF_LINES {
                                truncated = true;
                                break 'files;
                            }
                            rendered += 1;
                            let (text_color, background) = match line.kind {
                                DiffLineKind::Added => (
                                    status_colors.created,
                                    Some(status_colors.created_background),
                                ),
                                DiffLineKind::Removed => (
                                    status_colors.deleted,
                                    Some(status_colors.deleted_background),
                                ),
                                DiffLineKind::Hunk | DiffLineKind::Meta => {
                                    (colors.text_muted, None)
                                }
                                DiffLineKind::Context => (colors.text, None),
                            };
                            let prefix = diff_line_prefix(line.kind);
                            body = body.child(
                                div()
                                    .px_1()
                                    .whitespace_nowrap()
                                    .text_color(text_color)
                                    .when_some(background, |this, background| this.bg(background))
                                    .child(SharedString::from(format!("{prefix}{}", line.content))),
                            );
                        }
                    }
                }
                if truncated {
                    body = body.child(
                        Label::new(format!("… diff truncated at {MAX_DIFF_LINES} lines"))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    );
                }
            }
        }

        v_flex()
            .flex_1()
            .h_full()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .bg(colors.panel_background)
                    .child(Label::new(header).size(LabelSize::Small)),
            )
            .child(body)
    }

    /// The history pane: the painted branch graph in a fixed-width gutter,
    /// aligned row-for-row with a scrollable, selectable commit list.
    fn render_history(&self, cx: &Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let accents = cx.theme().accents();
        // A stable palette of distinct lane colors pulled from the active theme.
        let palette: Vec<Hsla> = (0..12u32).map(|i| accents.color_for_index(i)).collect();
        let metrics = GraphMetrics::default();

        let body = match (&self.history, &self.graph_layout) {
            (Some(history), Some(layout)) if !history.commits.is_empty() => {
                let graph = graph::commit_graph(
                    layout.clone(),
                    palette.clone(),
                    colors.panel_background,
                    metrics,
                );
                let rows = history
                    .commits
                    .iter()
                    .enumerate()
                    .map(|(i, commit)| self.render_commit_row(i, commit, &palette, metrics, cx))
                    .collect::<Vec<_>>();
                h_flex()
                    .items_start()
                    .child(div().flex_none().child(graph))
                    .child(v_flex().flex_1().children(rows))
                    .into_any_element()
            }
            _ => {
                let label = if self.history_loading {
                    "Loading history…"
                } else {
                    "No commit history."
                };
                div()
                    .p_3()
                    .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
                    .into_any_element()
            }
        };

        v_flex()
            .id("gitview-history")
            .h_full()
            .w(px(560.))
            .flex_none()
            .border_r_1()
            .border_color(colors.border)
            .bg(colors.panel_background)
            .child(
                h_flex()
                    .flex_none()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(colors.border)
                    .child(
                        Label::new("History")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(self.history.as_ref(), |this, history| {
                        this.child(
                            Label::new(format!("{} commits", history.commits.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(
                div()
                    .id("gitview-history-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .child(body),
            )
    }

    /// One commit row, sized to the graph's row height so its text lines up with
    /// the painted dot. Shows ref badges, subject, short id, author and age.
    fn render_commit_row(
        &self,
        _index: usize,
        commit: &CommitSummary,
        _palette: &[Hsla],
        metrics: GraphMetrics,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let colors = cx.theme().colors();
        let buffer_font = ThemeSettings::get_global(cx).buffer_font.family.clone();
        let selected = self.selected_commit.as_deref() == Some(commit.id.as_str());
        let row_id = SharedString::from(format!("commit-{}", commit.id));
        let commit_id = commit.id.clone();

        // Up to two branch/tag decoration badges.
        let badges = commit
            .refs
            .iter()
            .take(2)
            .map(|reference| {
                div()
                    .flex_none()
                    .px_1()
                    .rounded_sm()
                    .bg(colors.element_background)
                    .border_1()
                    .border_color(colors.border)
                    .child(
                        Label::new(ref_badge_label(reference))
                            .size(LabelSize::Small)
                            .color(Color::Accent),
                    )
                    .into_any_element()
            })
            .collect::<Vec<_>>();

        h_flex()
            .id(row_id)
            .h(metrics.row_height)
            .w_full()
            .items_center()
            .gap_2()
            .px_2()
            .cursor_pointer()
            .when(selected, |this| this.bg(colors.element_selected))
            .hover(|this| this.bg(colors.element_hover))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.select_commit(commit_id.clone(), cx);
            }))
            .children(badges)
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(Label::new(commit.subject.clone()).size(LabelSize::Small)),
            )
            .child(
                div().flex_none().font_family(buffer_font).child(
                    Label::new(commit.short_id.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                Label::new(commit.author_name.clone())
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Label::new(relative_age(commit.author_time))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
}

impl EventEmitter<GitViewEvent> for GitViewItem {}

impl Focusable for GitViewItem {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for GitViewItem {
    type Event = GitViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Git".into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::GitBranch))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("GitView Opened")
    }
}

impl Render for GitViewItem {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();

        v_flex()
            .key_context("GitView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(colors.editor_background)
            .text_color(colors.text)
            .child(self.render_top_bar(cx))
            .when_some(self.error.clone(), |this, error| {
                let status_colors = cx.theme().status();
                this.child(
                    div()
                        .w_full()
                        .flex_none()
                        .px_3()
                        .py_1p5()
                        .bg(status_colors.error_background)
                        .text_color(status_colors.error)
                        .child(SharedString::from(error)),
                )
            })
            .child(match self.view {
                ViewMode::Changes => h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_sidebar(cx))
                    .child(self.render_diff(cx)),
                ViewMode::History => h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.render_history(cx))
                    .child(self.render_diff(cx)),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_git_core::models::StatusKind;

    fn file(path: &str, bucket: StatusBucket) -> FileStatus {
        FileStatus {
            path: path.to_string(),
            old_path: None,
            index_status: "M".into(),
            worktree_status: ".".into(),
            kind: StatusKind::Ordinary,
            bucket,
        }
    }

    fn status_with_staged(paths: &[&str]) -> RepoStatus {
        let mut status = RepoStatus::empty("repo".to_string());
        status.staged = paths
            .iter()
            .map(|path| file(path, StatusBucket::Staged))
            .collect();
        status
    }

    #[test]
    fn diff_prefixes_match_line_kind() {
        assert_eq!(diff_line_prefix(DiffLineKind::Added), "+");
        assert_eq!(diff_line_prefix(DiffLineKind::Removed), "-");
        assert_eq!(diff_line_prefix(DiffLineKind::Context), " ");
        assert_eq!(diff_line_prefix(DiffLineKind::Hunk), "");
        assert_eq!(diff_line_prefix(DiffLineKind::Meta), "");
    }

    #[test]
    fn has_staged_changes_detects_staged_files() {
        assert!(!has_staged_changes(None));
        assert!(!has_staged_changes(Some(&RepoStatus::empty(
            "repo".to_string()
        ))));
        assert!(has_staged_changes(Some(&status_with_staged(&["a.rs"]))));
    }

    #[test]
    fn commit_block_reason_reports_each_condition() {
        let status = status_with_staged(&["a.rs"]);
        assert!(
            commit_block_reason(false, Some(&status), "message")
                .unwrap()
                .contains("not trusted")
        );
        assert!(
            commit_block_reason(true, None, "message")
                .unwrap()
                .contains("No staged")
        );
        assert!(
            commit_block_reason(true, Some(&status), "   \n\t")
                .unwrap()
                .contains("commit message")
        );
        assert_eq!(commit_block_reason(true, Some(&status), "fix: bug"), None);
    }

    #[test]
    fn bucket_staged_flag_finds_each_bucket() {
        let mut status = RepoStatus::empty("repo".to_string());
        status.staged = vec![file("staged.rs", StatusBucket::Staged)];
        status.unstaged = vec![file("unstaged.rs", StatusBucket::Unstaged)];
        status.untracked = vec![file("new.rs", StatusBucket::Untracked)];
        status.conflicted = vec![file("conflict.rs", StatusBucket::Conflicted)];

        assert_eq!(bucket_staged_flag(&status, "staged.rs"), Some(true));
        assert_eq!(bucket_staged_flag(&status, "unstaged.rs"), Some(false));
        assert_eq!(bucket_staged_flag(&status, "new.rs"), Some(false));
        assert_eq!(bucket_staged_flag(&status, "conflict.rs"), Some(false));
        assert_eq!(bucket_staged_flag(&status, "missing.rs"), None);
    }

    #[test]
    fn stageable_paths_collects_unstaged_and_untracked_deduped() {
        let mut status = RepoStatus::empty("repo".to_string());
        status.unstaged = vec![
            file("a.rs", StatusBucket::Unstaged),
            file("b.rs", StatusBucket::Unstaged),
        ];
        status.untracked = vec![
            file("c.rs", StatusBucket::Untracked),
            file("a.rs", StatusBucket::Untracked),
        ];
        status.staged = vec![file("d.rs", StatusBucket::Staged)];

        let paths = stageable_paths(&status);
        assert_eq!(
            paths,
            vec!["a.rs".to_string(), "b.rs".to_string(), "c.rs".to_string()]
        );
        assert!(!paths.contains(&"d.rs".to_string()));
    }

    #[test]
    fn unstageable_paths_collects_staged_only() {
        let status = status_with_staged(&["x.rs", "y.rs"]);
        assert_eq!(
            unstageable_paths(&status),
            vec!["x.rs".to_string(), "y.rs".to_string()]
        );
    }

    #[test]
    fn ref_badge_label_strips_decoration_prefixes() {
        assert_eq!(ref_badge_label("HEAD -> main"), "main");
        assert_eq!(ref_badge_label("tag: v1.0"), "v1.0");
        assert_eq!(ref_badge_label("origin/main"), "origin/main");
    }

    #[test]
    fn relative_age_formats_each_bucket() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(relative_age(now), "now");
        assert_eq!(relative_age(now - 120), "2m");
        assert_eq!(relative_age(now - 3 * 3600), "3h");
        assert_eq!(relative_age(now - 2 * 86400), "2d");
    }
}
