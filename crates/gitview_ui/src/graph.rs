//! Commit-graph layout + painter for the GitView history view.
//!
//! The engine (`lean_git_core`) assigns every commit a `lane` and a set of
//! commit->parent `graph_edges`, but those edges only describe a commit's *own*
//! parent links — they do not record which lanes merely *pass through* a row on
//! their way to a parent further down. Drawing straight from `graph_edges` would
//! therefore produce broken spines. So here we re-derive a continuous, per-row
//! routing (`GraphLayout`) from each commit's already-assigned `lane` + its
//! `parents`, mirroring the engine's lane-assignment algorithm exactly so lane
//! numbers stay consistent.
//!
//! The result is a list of rows, each with line **segments** (pass-through,
//! into-dot, out-of-dot) and a commit dot, painted with a `gpui::canvas` using
//! cubic-bezier S-curves for branch transitions — the GitLens / VS Code look.

use std::sync::Arc;

use gpui::{
    BorderStyle, Bounds, Canvas, Corners, Edges, Hsla, IntoElement, Pixels, Styled, canvas, point,
    px, quad, size,
};
use lean_git_core::models::CommitSummary;

/// How a single line segment travels through one commit row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// A lane that continues straight down through this row (top edge to bottom
    /// edge) without touching the commit dot.
    PassThrough,
    /// A line arriving at this commit's dot from above: top edge -> dot center.
    IntoDot,
    /// A line leaving this commit's dot toward a parent: dot center -> bottom
    /// edge.
    OutOfDot,
}

/// One line segment within a row, addressed by lane indices (resolved to x
/// pixel positions at paint time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphSegment {
    pub kind: SegmentKind,
    /// Lane position at the top of the segment.
    pub from_lane: usize,
    /// Lane position at the bottom of the segment.
    pub to_lane: usize,
    /// Palette index used to color this segment (keeps each lane's color
    /// continuous across rows).
    pub color: usize,
}

/// A single commit row's drawable graph contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRowLayout {
    pub dot_lane: usize,
    pub dot_color: usize,
    pub segments: Vec<GraphSegment>,
}

/// The full graph routing for a page of commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphLayout {
    pub rows: Vec<GraphRowLayout>,
    pub lane_count: usize,
}

/// Fixed pixel metrics for the graph column. Row height must match the commit
/// row height used by the list so dots line up with their text.
#[derive(Debug, Clone, Copy)]
pub struct GraphMetrics {
    pub row_height: Pixels,
    pub lane_width: Pixels,
    pub gutter: Pixels,
    pub dot_diameter: Pixels,
    pub stroke_width: Pixels,
}

impl Default for GraphMetrics {
    fn default() -> Self {
        Self {
            row_height: px(36.),
            lane_width: px(14.),
            gutter: px(10.),
            dot_diameter: px(9.),
            stroke_width: px(1.5),
        }
    }
}

impl GraphMetrics {
    /// Total width of the graph column for the given lane count.
    pub fn width(&self, lane_count: usize) -> Pixels {
        self.gutter * 2. + self.lane_width * lane_count.max(1) as f32
    }
}

/// Re-derive continuous per-row graph routing from the engine-assigned lanes and
/// each commit's parents. The algorithm mirrors `assign_graph_lanes` in the
/// engine: the first parent continues in the commit's lane, additional (merge)
/// parents take an existing or freshly-allocated lane.
pub fn compute_graph_layout(commits: &[CommitSummary]) -> GraphLayout {
    // `active[lane]` is the commit id that lane is currently waiting to reach.
    let mut active: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    let mut max_lane = 0usize;

    for commit in commits {
        let lane = commit.lane;
        if active.len() <= lane {
            active.resize(lane + 1, None);
        }

        // Lines entering this row from above.
        let top = active.clone();
        let mut segments = Vec::new();
        let mut out_targets: Vec<usize> = Vec::new();

        // Every lane that was waiting for this commit converges into the dot.
        for (l, slot) in top.iter().enumerate() {
            if slot.as_deref() == Some(commit.id.as_str()) {
                segments.push(GraphSegment {
                    kind: SegmentKind::IntoDot,
                    from_lane: l,
                    to_lane: lane,
                    // The commit's own lane keeps the dot color; a branch merging
                    // in from another lane keeps that branch's color.
                    color: if l == lane { lane } else { l },
                });
            }
        }

        // Those convergent lanes are now consumed.
        for slot in active.iter_mut() {
            if slot.as_deref() == Some(commit.id.as_str()) {
                *slot = None;
            }
        }

        // Route parents into the lanes that leave the bottom of this row.
        if let Some((first_parent, merge_parents)) = commit.parents.split_first() {
            active[lane] = Some(first_parent.clone());
            segments.push(GraphSegment {
                kind: SegmentKind::OutOfDot,
                from_lane: lane,
                to_lane: lane,
                color: lane,
            });
            out_targets.push(lane);

            for parent in merge_parents {
                let target = allocate_lane(&mut active, parent);
                segments.push(GraphSegment {
                    kind: SegmentKind::OutOfDot,
                    from_lane: lane,
                    to_lane: target,
                    color: target,
                });
                out_targets.push(target);
            }
        } else {
            // Root commit: its lane ends here.
            active[lane] = None;
        }

        let bottom = &active;

        // Any lane occupied identically above and below, that wasn't consumed by
        // or emitted from the dot, is a straight pass-through spine.
        let span = top.len().max(bottom.len());
        for l in 0..span {
            if l == lane || out_targets.contains(&l) {
                continue;
            }
            let t = top.get(l).and_then(Option::as_deref);
            let b = bottom.get(l).and_then(Option::as_deref);
            if let (Some(t_id), Some(b_id)) = (t, b) {
                if t_id == b_id && t_id != commit.id {
                    segments.push(GraphSegment {
                        kind: SegmentKind::PassThrough,
                        from_lane: l,
                        to_lane: l,
                        color: l,
                    });
                }
            }
        }

        max_lane = max_lane.max(lane);
        for segment in &segments {
            max_lane = max_lane.max(segment.from_lane).max(segment.to_lane);
        }

        rows.push(GraphRowLayout {
            dot_lane: lane,
            dot_color: lane,
            segments,
        });
    }

    GraphLayout {
        rows,
        lane_count: if commits.is_empty() { 0 } else { max_lane + 1 },
    }
}

/// Find the lane currently waiting for `parent`, or claim the first free lane,
/// or append a new one. Mirrors the engine's allocation order.
fn allocate_lane(active: &mut Vec<Option<String>>, parent: &str) -> usize {
    if let Some(existing) = active
        .iter()
        .position(|slot| slot.as_deref() == Some(parent))
    {
        return existing;
    }
    if let Some(free) = active.iter().position(Option::is_none) {
        active[free] = Some(parent.to_string());
        return free;
    }
    active.push(Some(parent.to_string()));
    active.len() - 1
}

/// Build the painted graph column element for a computed layout.
///
/// Colors are resolved up front (the paint closure only has `&App`), so the
/// palette / panel background are passed in by the caller.
pub fn commit_graph(
    layout: Arc<GraphLayout>,
    palette: Vec<Hsla>,
    panel_bg: Hsla,
    metrics: GraphMetrics,
) -> impl IntoElement {
    let lane_count = layout.lane_count;
    let row_count = layout.rows.len();
    let width = metrics.width(lane_count);
    let height = metrics.row_height * row_count.max(1) as f32;

    let color_at = move |index: usize| -> Hsla {
        if palette.is_empty() {
            panel_bg
        } else {
            palette[index % palette.len()]
        }
    };

    let painter: Canvas<()> = canvas(
        move |_bounds, _window, _cx| {},
        move |bounds, _state, window, _cx| {
            let origin = bounds.origin;
            let row_h = metrics.row_height;
            let lane_w = metrics.lane_width;
            let gutter = metrics.gutter;

            let lane_x =
                |lane: usize| -> Pixels { origin.x + gutter + lane_w * lane as f32 + lane_w / 2. };

            for (i, row) in layout.rows.iter().enumerate() {
                let row_top = origin.y + row_h * i as f32;
                let y_top = row_top;
                let y_center = row_top + row_h / 2.;
                let y_bottom = row_top + row_h;

                for segment in &row.segments {
                    let color = color_at(segment.color);
                    let mut path = gpui::PathBuilder::stroke(metrics.stroke_width);
                    let x_from = lane_x(segment.from_lane);
                    let x_to = lane_x(segment.to_lane);
                    match segment.kind {
                        SegmentKind::PassThrough => {
                            path.move_to(point(x_from, y_top));
                            path.line_to(point(x_from, y_bottom));
                        }
                        SegmentKind::IntoDot => {
                            path.move_to(point(x_from, y_top));
                            if x_from == x_to {
                                path.line_to(point(x_to, y_center));
                            } else {
                                let mid = row_top + row_h / 4.;
                                path.cubic_bezier_to(
                                    point(x_to, y_center),
                                    point(x_from, mid),
                                    point(x_to, mid),
                                );
                            }
                        }
                        SegmentKind::OutOfDot => {
                            path.move_to(point(x_from, y_center));
                            if x_from == x_to {
                                path.line_to(point(x_to, y_bottom));
                            } else {
                                let mid = row_top + row_h * 3. / 4.;
                                path.cubic_bezier_to(
                                    point(x_to, y_bottom),
                                    point(x_from, mid),
                                    point(x_to, mid),
                                );
                            }
                        }
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                }

                // The commit dot: a filled circle ringed with the panel
                // background so spines read as passing behind it.
                let d = metrics.dot_diameter;
                let cx_ = lane_x(row.dot_lane);
                let dot_bounds = Bounds::new(point(cx_ - d / 2., y_center - d / 2.), size(d, d));
                window.paint_quad(quad(
                    dot_bounds,
                    Corners::all(d / 2.),
                    color_at(row.dot_color),
                    Edges::all(px(1.5)),
                    panel_bg,
                    BorderStyle::Solid,
                ));
            }
        },
    );

    painter.w(width).h(height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_git_core::git::history::parse_history_page;

    fn commit(id: &str, parents: &[&str], lane: usize) -> CommitSummary {
        CommitSummary {
            id: id.to_string(),
            short_id: id.chars().take(8).collect(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author_name: "Tester".into(),
            author_time: 0,
            refs: Vec::new(),
            subject: format!("commit {id}"),
            lane,
            graph_edges: Vec::new(),
        }
    }

    #[test]
    fn linear_history_is_a_single_spine() {
        // c -> b -> a, all in lane 0.
        let commits = vec![
            commit("c", &["b"], 0),
            commit("b", &["a"], 0),
            commit("a", &[], 0),
        ];
        let layout = compute_graph_layout(&commits);
        assert_eq!(layout.lane_count, 1);
        // Every dot in lane 0.
        assert!(layout.rows.iter().all(|row| row.dot_lane == 0));
        // No pass-through lines are needed for a straight line.
        assert!(layout.rows.iter().all(|row| {
            row.segments
                .iter()
                .all(|s| s.kind != SegmentKind::PassThrough)
        }));
        // The first commit emits one out-of-dot toward its parent; the root
        // commit emits none.
        let out = |row: &GraphRowLayout| {
            row.segments
                .iter()
                .filter(|s| s.kind == SegmentKind::OutOfDot)
                .count()
        };
        assert_eq!(out(&layout.rows[0]), 1);
        assert_eq!(out(&layout.rows[2]), 0);
    }

    #[test]
    fn passthrough_lane_is_continuous_across_an_intermediate_commit() {
        // tip(0) has parents [base, side]; side(1) sits between tip and base in
        // the list, so while we render `mid`(0) the side branch (lane 1) must
        // pass straight through.
        //
        //   tip   (lane 0) parents: mid, side
        //   mid   (lane 0) parents: base
        //   side  (lane 1) parents: base
        //   base  (lane 0) parents: -
        let commits = vec![
            commit("tip", &["mid", "side"], 0),
            commit("mid", &["base"], 0),
            commit("side", &["base"], 1),
            commit("base", &[], 0),
        ];
        let layout = compute_graph_layout(&commits);
        assert!(layout.lane_count >= 2);
        // Row 1 (`mid`) must carry a pass-through for lane 1 (the side branch
        // travelling down to `base`).
        let mid = &layout.rows[1];
        assert!(
            mid.segments
                .iter()
                .any(|s| s.kind == SegmentKind::PassThrough && s.from_lane == 1),
            "intermediate row should pass the side branch through: {mid:?}"
        );
    }

    #[test]
    fn merge_commit_has_two_out_of_dot_segments() {
        let commits = vec![
            commit("merge", &["main", "feat"], 0),
            commit("feat", &["base"], 1),
            commit("main", &["base"], 0),
            commit("base", &[], 0),
        ];
        let layout = compute_graph_layout(&commits);
        let merge = &layout.rows[0];
        let out: Vec<_> = merge
            .segments
            .iter()
            .filter(|s| s.kind == SegmentKind::OutOfDot)
            .collect();
        assert_eq!(out.len(), 2, "merge should fork to two parents: {merge:?}");
        assert!(out.iter().any(|s| s.to_lane != merge.dot_lane));
    }

    #[test]
    fn layout_matches_engine_lane_assignment_on_real_parse() {
        // Build a tiny real history through the engine parser, then ensure our
        // dot lanes match the engine's assigned lanes exactly.
        let stdout = b"merge\x1fmain feat\x1fA\x1f3\x1f\x1fmerge\n\
                       feat\x1fbase\x1fA\x1f2\x1f\x1ffeature\n\
                       main\x1fbase\x1fA\x1f1\x1f\x1fmain\n\
                       base\x1f\x1fA\x1f0\x1f\x1finit\n";
        let page = parse_history_page(&"repo".to_string(), stdout, 50).unwrap();
        let layout = compute_graph_layout(&page.commits);
        assert_eq!(layout.rows.len(), page.commits.len());
        for (row, commit) in layout.rows.iter().zip(&page.commits) {
            assert_eq!(row.dot_lane, commit.lane);
        }
    }
}
