mod gitview_item;
mod graph;

pub use gitview_item::GitViewItem;

use gpui::{App, actions};
use workspace::Workspace;

actions!(
    gitview,
    [
        /// Opens the GitView panel for the repository open in the workspace.
        OpenGitView
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenGitView, window, cx| {
            GitViewItem::open(workspace, window, cx);
        });
    })
    .detach();
}
