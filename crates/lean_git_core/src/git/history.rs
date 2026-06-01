use crate::error::{AppError, AppResult};
use crate::models::{CommitSummary, GraphEdge, HistoryPage, RepoId};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const SEP: char = '\x1f';

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryCursor {
    pub offset: usize,
    pub lanes: Vec<Option<String>>,
    #[serde(default)]
    pub anchor: Option<String>,
}

pub fn decode_history_cursor(input: Option<&str>) -> HistoryCursor {
    let Some(input) = input else {
        return HistoryCursor::default();
    };
    if let Ok(offset) = input.parse::<usize>() {
        return HistoryCursor {
            offset,
            lanes: Vec::new(),
            anchor: None,
        };
    }
    serde_json::from_str(input).unwrap_or_default()
}

pub fn parse_history_page(repo_id: &RepoId, input: &[u8], limit: usize) -> AppResult<HistoryPage> {
    parse_history_page_with_cursor(repo_id, input, limit, HistoryCursor::default())
}

pub fn parse_history_page_with_cursor(
    repo_id: &RepoId,
    input: &[u8],
    limit: usize,
    cursor: HistoryCursor,
) -> AppResult<HistoryPage> {
    let text = String::from_utf8_lossy(input);
    let rows = parse_raw_rows(&text)?;
    let first_parent_by_id = first_parent_map(&rows);
    let mut commits = Vec::new();
    let mut lanes = LaneState::new(cursor.lanes);
    let mut lane_count = lanes.len();

    for row in rows.iter() {
        if commits.len() >= limit {
            break;
        }
        let layout = lanes.assign(&row.id, &row.parents, &first_parent_by_id);
        lane_count = lane_count.max(layout.lane + 1);
        for edge in &layout.graph_edges {
            lane_count = lane_count.max(edge.from_lane + 1).max(edge.to_lane + 1);
        }
        lane_count = lane_count.max(lanes.len());
        commits.push(CommitSummary {
            short_id: row.id.chars().take(8).collect(),
            id: row.id.clone(),
            parents: row.parents.clone(),
            author_name: row.author_name.clone(),
            author_time: row.author_time,
            refs: row.refs.clone(),
            subject: row.subject.clone(),
            lane: layout.lane,
            graph_edges: layout.graph_edges,
        });
    }

    let truncated = rows.len() > commits.len();
    let next_cursor = truncated.then(|| {
        encode_history_cursor(&HistoryCursor {
            offset: cursor.offset + commits.len(),
            lanes: lanes.into_lanes(),
            anchor: cursor
                .anchor
                .or_else(|| commits.first().map(|commit| commit.id.clone())),
        })
    });
    Ok(HistoryPage {
        repo_id: repo_id.clone(),
        commits,
        lane_count,
        next_cursor,
        truncated,
    })
}

#[derive(Debug)]
struct RawCommitRow {
    id: String,
    parents: Vec<String>,
    author_name: String,
    author_time: i64,
    refs: Vec<String>,
    subject: String,
}

fn parse_raw_rows(text: &str) -> AppResult<Vec<RawCommitRow>> {
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let fields: Vec<&str> = line.splitn(6, SEP).collect();
        if fields.len() < 6 {
            return Err(AppError::new(
                "parse_history",
                "commit row has too few fields",
            ));
        }
        rows.push(RawCommitRow {
            id: fields[0].to_string(),
            parents: fields[1]
                .split_whitespace()
                .filter(|parent| !parent.is_empty())
                .map(ToString::to_string)
                .collect(),
            author_name: fields[2].to_string(),
            author_time: fields[3].parse().unwrap_or(0),
            refs: parse_refs(fields[4]),
            subject: fields[5].to_string(),
        });
    }
    Ok(rows)
}

fn first_parent_map(rows: &[RawCommitRow]) -> HashMap<String, String> {
    rows.iter()
        .filter_map(|row| {
            row.parents
                .first()
                .map(|parent| (row.id.clone(), parent.clone()))
        })
        .collect()
}

fn first_parent_chain_contains(
    start: &str,
    target: &str,
    first_parent_by_id: &HashMap<String, String>,
) -> bool {
    let mut current = start;
    let mut visited = HashSet::new();
    while visited.insert(current.to_string()) {
        if current == target {
            return true;
        }
        let Some(parent) = first_parent_by_id.get(current) else {
            return false;
        };
        current = parent;
    }
    false
}

fn encode_history_cursor(cursor: &HistoryCursor) -> String {
    serde_json::to_string(cursor).unwrap_or_else(|_| cursor.offset.to_string())
}

fn parse_refs(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Default)]
struct LaneState {
    lanes: Vec<Option<String>>,
}

struct CommitLayout {
    lane: usize,
    graph_edges: Vec<GraphEdge>,
}

#[derive(Clone, Copy)]
struct ResolvedParentLane {
    parent_index: usize,
    lane: usize,
    claim_parent: bool,
    color_lane: usize,
}

impl LaneState {
    fn new(lanes: Vec<Option<String>>) -> Self {
        Self { lanes }
    }

    fn len(&self) -> usize {
        self.lanes.len()
    }

    fn into_lanes(self) -> Vec<Option<String>> {
        self.lanes
    }

    fn assign(
        &mut self,
        commit: &str,
        parents: &[String],
        first_parent_by_id: &HashMap<String, String>,
    ) -> CommitLayout {
        let lane = self
            .find_lane(commit)
            .unwrap_or_else(|| self.first_empty_lane(0));
        self.ensure_lane(lane);

        let parent_lanes = self.resolve_parent_lanes(lane, parents, first_parent_by_id);
        let mut graph_edges = self.continuation_edges(lane);
        for parent in &parent_lanes {
            graph_edges.push(GraphEdge {
                from_lane: lane,
                to_lane: parent.lane,
                parent_index: Some(parent.parent_index.min(u8::MAX as usize) as u8),
                color_index: (parent.color_lane % 8) as u8,
            });
        }

        self.remove_commit(commit);
        for parent in &parent_lanes {
            if parent.claim_parent {
                self.remove_commit(&parents[parent.parent_index]);
            }
        }
        for parent in parent_lanes {
            if parent.claim_parent {
                self.ensure_lane(parent.lane);
                self.lanes[parent.lane] = Some(parents[parent.parent_index].clone());
            }
        }
        self.prune();

        CommitLayout { lane, graph_edges }
    }

    fn resolve_parent_lanes(
        &mut self,
        commit_lane: usize,
        parents: &[String],
        first_parent_by_id: &HashMap<String, String>,
    ) -> Vec<ResolvedParentLane> {
        let mut resolved = Vec::with_capacity(parents.len());
        let mut reserved = Vec::new();
        for (index, parent) in parents.iter().enumerate() {
            let parent_lane = if index == 0 {
                if let Some(existing) = self.find_lane(parent) {
                    ResolvedParentLane {
                        parent_index: index,
                        lane: existing,
                        claim_parent: existing == commit_lane,
                        color_lane: if existing < commit_lane {
                            commit_lane
                        } else {
                            existing
                        },
                    }
                } else if let Some(lower_lane) =
                    self.lower_lane_reaches_parent(commit_lane, parent, first_parent_by_id)
                {
                    ResolvedParentLane {
                        parent_index: index,
                        lane: lower_lane,
                        claim_parent: false,
                        color_lane: commit_lane,
                    }
                } else {
                    ResolvedParentLane {
                        parent_index: index,
                        lane: commit_lane,
                        claim_parent: true,
                        color_lane: commit_lane,
                    }
                }
            } else if let Some(existing) = self.find_lane(parent) {
                ResolvedParentLane {
                    parent_index: index,
                    lane: existing,
                    claim_parent: false,
                    color_lane: existing,
                }
            } else {
                let lane = self.first_unreserved_empty_lane(commit_lane + 1, &reserved);
                ResolvedParentLane {
                    parent_index: index,
                    lane,
                    claim_parent: true,
                    color_lane: lane,
                }
            };
            reserved.push(parent_lane.lane);
            resolved.push(parent_lane);
        }
        resolved
    }

    fn lower_lane_reaches_parent(
        &self,
        commit_lane: usize,
        parent: &str,
        first_parent_by_id: &HashMap<String, String>,
    ) -> Option<usize> {
        self.lanes
            .iter()
            .enumerate()
            .take(commit_lane)
            .find_map(|(lane, active)| {
                active
                    .as_deref()
                    .filter(|active| {
                        first_parent_chain_contains(active, parent, first_parent_by_id)
                    })
                    .map(|_| lane)
            })
    }

    fn continuation_edges(&self, commit_lane: usize) -> Vec<GraphEdge> {
        self.lanes
            .iter()
            .enumerate()
            .filter(|(lane, commit)| *lane != commit_lane && commit.is_some())
            .map(|(lane, _)| GraphEdge {
                from_lane: lane,
                to_lane: lane,
                parent_index: None,
                color_index: (lane % 8) as u8,
            })
            .collect()
    }

    fn find_lane(&self, commit: &str) -> Option<usize> {
        self.lanes
            .iter()
            .position(|lane_commit| lane_commit.as_deref() == Some(commit))
    }

    fn first_empty_lane(&mut self, start: usize) -> usize {
        if let Some(index) = self
            .lanes
            .iter()
            .enumerate()
            .skip(start)
            .find_map(|(index, commit)| commit.is_none().then_some(index))
        {
            index
        } else {
            self.lanes.push(None);
            self.lanes.len() - 1
        }
    }

    fn first_unreserved_empty_lane(&mut self, start: usize, reserved: &[usize]) -> usize {
        if let Some(index) =
            self.lanes
                .iter()
                .enumerate()
                .skip(start)
                .find_map(|(index, commit)| {
                    (commit.is_none() && !reserved.contains(&index)).then_some(index)
                })
        {
            index
        } else {
            self.lanes.push(None);
            self.lanes.len() - 1
        }
    }

    fn ensure_lane(&mut self, lane: usize) {
        while self.lanes.len() <= lane {
            self.lanes.push(None);
        }
    }

    fn remove_commit(&mut self, commit: &str) {
        for lane in &mut self.lanes {
            if lane.as_deref() == Some(commit) {
                *lane = None;
            }
        }
    }

    fn prune(&mut self) {
        while self.lanes.last().is_some_and(Option::is_none) {
            self.lanes.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_history_rows_refs_and_graph_edges() {
        let input = b"aaaaaaaa\x1fbbbbbbbb cccccccc\x1fAda\x1f1710000000\x1fHEAD -> main, tag: v1\x1fmerge subject\nbbbbbbbb\x1f\x1fAda\x1f1709999999\x1f\x1froot\n";
        let page = parse_history_page(&"repo".to_string(), input, 200).unwrap();
        assert_eq!(page.commits.len(), 2);
        assert_eq!(page.commits[0].parents, vec!["bbbbbbbb", "cccccccc"]);
        assert_eq!(page.commits[0].refs, vec!["HEAD -> main", "tag: v1"]);
        assert_eq!(page.commits[1].lane, page.commits[0].lane);
        assert_eq!(page.commits[0].graph_edges.len(), 2);
        assert!(page.lane_count >= 2);
    }

    #[test]
    fn linear_history_stays_on_lane_zero() {
        let input = b"c\x1fp\x1fA\x1f1\x1f\x1fchild\np\x1f\x1fA\x1f1\x1f\x1fparent\n";
        let page = parse_history_page(&"repo".to_string(), input, 20).unwrap();
        assert_eq!(
            page.commits
                .iter()
                .map(|commit| commit.lane)
                .collect::<Vec<_>>(),
            vec![0, 0]
        );
        assert_eq!(page.commits[0].graph_edges[0].from_lane, 0);
        assert_eq!(page.commits[0].graph_edges[0].to_lane, 0);
        assert!(page.commits[1].graph_edges.is_empty());
    }

    #[test]
    fn merge_edges_include_diagonal_parent() {
        let input = b"m\x1fa b\x1fA\x1f1\x1f\x1fmerge\na\x1fr\x1fA\x1f1\x1f\x1fmain\nb\x1fr\x1fA\x1f1\x1f\x1fside\nr\x1f\x1fA\x1f1\x1f\x1froot\n";
        let page = parse_history_page(&"repo".to_string(), input, 20).unwrap();
        assert!(
            page.commits[0]
                .graph_edges
                .iter()
                .any(|edge| edge.from_lane == 0 && edge.to_lane == 1)
        );
    }

    #[test]
    fn first_parent_stays_on_current_lane_when_root_is_active_on_side_lane() {
        let input = b"m\x1fa b\x1fA\x1f1\x1f\x1fmerge\nb\x1fr\x1fA\x1f1\x1f\x1fside\na\x1fr\x1fA\x1f1\x1f\x1fmain\nr\x1f\x1fA\x1f1\x1f\x1froot\n";
        let page = parse_history_page(&"repo".to_string(), input, 20).unwrap();
        let a = page.commits.iter().find(|commit| commit.id == "a").unwrap();
        assert!(a.graph_edges.iter().any(|edge| {
            edge.parent_index == Some(0) && edge.from_lane == 0 && edge.to_lane == 0
        }));
    }

    #[test]
    fn side_branch_converges_to_existing_lower_parent_lane() {
        let input = b"m\x1fa b\x1fA\x1f1\x1f\x1fmerge\na\x1fr\x1fA\x1f1\x1f\x1fmain\nb\x1fr\x1fA\x1f1\x1f\x1fside\nr\x1f\x1fA\x1f1\x1f\x1froot\n";
        let page = parse_history_page(&"repo".to_string(), input, 20).unwrap();
        let side = page.commits.iter().find(|commit| commit.id == "b").unwrap();
        assert_eq!(side.lane, 1);
        assert!(side.graph_edges.iter().any(|edge| {
            edge.parent_index == Some(0)
                && edge.from_lane == 1
                && edge.to_lane == 0
                && edge.color_index == 1
        }));
        let root = page.commits.iter().find(|commit| commit.id == "r").unwrap();
        assert_eq!(root.lane, 0);
    }

    #[test]
    fn pagination_cursor_preserves_active_lanes() {
        let input = b"m\x1fa b\x1fA\x1f1\x1f\x1fmerge\na\x1fr\x1fA\x1f1\x1f\x1fmain\n";
        let first =
            parse_history_page_with_cursor(&"repo".to_string(), input, 1, HistoryCursor::default())
                .unwrap();
        let cursor = decode_history_cursor(first.next_cursor.as_deref());
        assert_eq!(cursor.offset, 1);
        assert_eq!(cursor.lanes.len(), 2);
        assert_eq!(cursor.anchor.as_deref(), Some("m"));

        let second_input = b"a\x1fr\x1fA\x1f1\x1f\x1fmain\nb\x1fr\x1fA\x1f1\x1f\x1fside\n";
        let second =
            parse_history_page_with_cursor(&"repo".to_string(), second_input, 1, cursor).unwrap();
        assert_eq!(second.commits[0].lane, 0);
        assert!(
            second.commits[0]
                .graph_edges
                .iter()
                .any(|edge| edge.parent_index.is_none() && edge.from_lane == 1)
        );
    }

    #[test]
    fn marks_truncated_when_limit_hit() {
        let input = b"a\x1f\x1fA\x1f1\x1f\x1fone\nb\x1f\x1fA\x1f1\x1f\x1ftwo\n";
        let page = parse_history_page(&"repo".to_string(), input, 1).unwrap();
        assert!(page.truncated);
        let cursor = decode_history_cursor(page.next_cursor.as_deref());
        assert_eq!(cursor.offset, 1);
        assert_eq!(cursor.anchor.as_deref(), Some("a"));
    }
}
