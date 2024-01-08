use super::{
    eval::{eval, penalty},
    state::State,
};
use crate::{
    config::Parameters,
    place::{Place, PlaceFinder},
    shape::ShapeTable,
};
use std::collections::{BinaryHeap, HashMap};

// Search algorithm

/// An instance of the "B*" search algorithm.
///
/// B* is a slight modification of A* to better fit the needs of blockfish. The primary
/// motivation for a new search algorithm, is that one must always be placing pieces; that
/// is, a good looking game state is not actually good if the following placements lead it
/// to being worse. Qhile A* has been good at finding game states with good evaluations,
/// it is not good at ruling them out if the subsequent placements are bad.
///
/// The gist of B* is that it does a best-first search to find full sequences (every piece
/// in the queue is placed), then tries to improve sequences by modifying mid-sequence
/// moves. Crucially, evaluations are only ever propogated back once the best-first search
/// has finished. This means a placement is only actually considered good if the full
/// sequence is good.
///
/// While A* contains a single fringe set (priority queue ordered by `f(n)` value), B*
/// works by keeping separate fringe sets for each depth level. That is, when a node at
/// level `i` is expanded, its successors are put into the fringe set for level `i+1`. The
/// algorithm starts with best-first search at level `0` with just the root node in the
/// fringe set. This phase proceeds by picking a node from depth level `i`, expanded it,
/// then repeating at depth level `i+1`, until a terminal node is found. This terminal
/// node's rating is backed up, then the best-first search begins again starting at the
/// node with lowest `f(n)` (among all depth levels).
///
/// NOTE: the final "rating" given to a terminal node† is not actually its `f(n)` value,
/// but rather `f(n) + f(n.parent)`. The reasoning for this, is that the final node in a
/// sequence is not actually the only placement available. Because of "hold", there may
/// actually be some other type of piece immediately beyond the queue that could be placed
/// instead. Adding the two f values rounds off the rating for sequences that were pretty
/// good until the last piece, since this last piece won't be the only option. This edge
/// condition will hopefully be removed once predicting-pieces-after-the-previews is
/// implemented.
///
/// † unless this node clears the bottom line of the matrix, i.e. "reaches the goal" -- in
///   this case the node will be given a very low rating proportional to the number of
///   placed pieces, in order to prioritize short sequences at the very end of the cheese
///   race.
pub struct Search<'s> {
    // parameters to the heuristic function
    params: Parameters,
    // holds the best rating for each move
    move_best: HashMap<MoveId, i64>,
    // fringe set for each depth level
    lvls: Vec<BinaryHeap<Node>>,
    // index of current depth level either being selected from or expanded into
    lvl_idx: usize,
    // current node being expanded
    node: Option<Node>,
    // placement generator; only used when `node` is not `None`
    pfind: PlaceFinder<'s>,
    // total number of nodes generated
    node_count: usize,
}

/// Opaque identifier that indicates a "move" -- the next placement one make after the
/// initial state.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MoveId(u8);

/// Indicates what happened as a result of a step of the algorithm. Returned by
/// `Search::step()`.
#[derive(Clone, Debug)]
pub enum Step {
    /// Indicates that the rating for a particular move has changed since the last iteration
    /// of the analysis.
    RatingChanged {
        /// Move whos rating has changed.
        move_id: MoveId,
        /// Placement trace for the new best-sequence for this move.
        trace: Vec<usize>,
        /// The new rating for this move.
        rating: i64,
    },

    /// Indicates a node was discovered but was rejected since it is not better than the
    /// current best node for the move.
    SequenceRejected {
        /// Placement trace for this sequence.
        trace: Vec<usize>,
        /// The sequence's rating.
        rating: i64,
    },

    Other,
}

/// Indicates that the search is over since there are no more placements left to analyze.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SearchTerminated;

impl<'s> Search<'s> {
    /// Constructs a new instance of the "B*" search algorithm.
    pub fn new(shape_table: &'s ShapeTable, params: Parameters) -> Self {
        Self {
            params,
            move_best: HashMap::with_capacity(64),
            lvls: Vec::with_capacity(8),
            lvl_idx: 0,
            node: None,
            pfind: PlaceFinder::new(shape_table),
            node_count: 0,
        }
    }

    /// Starts the search at `root_state`.
    pub fn start(&mut self, root_state: State) {
        for lvl in self.lvls.iter_mut() {
            lvl.clear();
        }
        self.lvl_idx = 0;
        root_state.placements(&mut self.pfind);

        self.node = Some(Node::root(&self.params, root_state));
        self.node_count = 1;
    }

    /// Returns the total number of generated nodes.
    pub fn node_count(&self) -> usize {
        self.node_count
    }

    /// Runs one iteration of the algorithm. Returns `Ok(Some(rc))` it move rating was
    /// modified, `Ok(None)` if work was performed but no ratings were modified yet, or
    /// `Err(SearchTerminated)` if there are no more nodes remaining to be processed.
    pub fn step(&mut self) -> Result<Step, SearchTerminated> {
        if let Some(node) = self.node.take() {
            // best-first iteration phase
            if node.is_terminal() {
                // stop at terminal nodes
                return Ok(match self.back_up(node) {
                    (rating, trace, Some(move_id)) => Step::RatingChanged {
                        move_id,
                        rating,
                        trace,
                    },
                    (rating, trace, None) => Step::SequenceRejected { rating, trace },
                });
            }
            // expansion
            if let Some(pl) = self.pfind.next() {
                self.push(node.succ(&self.params, &pl));
                self.node = Some(node);
            } else {
                self.pop()?;
            }
        } else {
            // reselection phase
            self.select();
            self.pop()?;
        }
        Ok(Step::Other)
    }

    pub fn raw_step(&mut self) -> Result<Option<u16>, SearchTerminated> {
        if let Some(node) = self.node.take() {
            // best-first iteration phase
            if node.is_terminal() {
                // stop at terminal nodes
                let estimate = node.estimate;
                self.back_up(node);
                return Ok(Some(estimate))
            }
            // expansion
            if let Some(pl) = self.pfind.next() {
                let succ = node.succ(&self.params, &pl);
                let estimate = succ.estimate;
                self.push(succ);
                self.node = Some(node);
                Ok(Some(estimate))
            } else {
                self.pop()?;
                Ok(None)
            }
        } else {
            // reselection phase
            self.select();
            self.pop()?;
            Ok(None)
        }
    }

    /// Adds `node` to the fringe set at the current level index.
    fn push(&mut self, node: Node) {
        let lvl = match self.lvls.get_mut(self.lvl_idx) {
            Some(lvl) => lvl,
            None => {
                self.lvls.resize_with(self.lvl_idx + 1, default_level);
                &mut self.lvls[self.lvl_idx]
            }
        };
        lvl.push(node);
        self.node_count += 1;
    }

    /// Removes the best node at the current level, initializes `self.node` and
    /// `self.pfind` to that node (for generating successors), then advances the level
    /// index.
    fn pop(&mut self) -> Result<(), SearchTerminated> {
        let lvl = self.lvls.get_mut(self.lvl_idx).ok_or(SearchTerminated)?;
        let node = lvl.pop().ok_or(SearchTerminated)?;
        self.node_count -= 1;
        node.state.placements(&mut self.pfind);
        self.node = Some(node);
        self.lvl_idx += 1;
        Ok(())
    }

    /// Selects the level index corresponding to the node with best evaluation.
    fn select(&mut self) {
        self.lvl_idx = (0..self.lvls.len())
            .min_by_key(|&i| self.lvls[i].peek().map_or(std::i64::MAX, |n| n.f))
            .unwrap_or(0);
    }

    /// Propogates `node`'s rating back to the move at the root of this node.
    fn back_up(&mut self, node: Node) -> (i64, Vec<usize>, Option<MoveId>) {
        let rating = node.rating();
        let trace = node.trace().collect();
        let move_id = match node.trace.get(0) {
            Some(&idx) => {
                let m_id = MoveId(idx as _);
                let best = self.move_best.entry(m_id).or_insert(std::i64::MAX);
                if rating < *best {
                    *best = rating;
                    Some(m_id)
                } else {
                    None
                }
            }
            None => None,
        };
        (rating, trace, move_id)
    }
}

fn default_level() -> BinaryHeap<Node> {
    BinaryHeap::with_capacity(1024)
}

impl MoveId {
    #[cfg(test)]
    pub fn n(x: i32) -> Self {
        Self(x as u8)
    }
}

// Nodes

struct Node {
    state: State,
    trace: Vec<u8>,
    f: i64,
    parent_f: i64,
    estimate: u16,
}

impl Node {
    fn root(params: &Parameters, state: State) -> Self {
        let (eval, e) = eval(state.matrix());
        let h = eval.score(params);
        Self {
            state,
            trace: vec![],
            f: h,
            parent_f: h,
            estimate: e
        }
    }

    /// Generates a successor node from this node, by placing `pl`. Uses `params` to
    /// compute the new evaluation.
    fn succ(&self, params: &Parameters, pl: &Place) -> Self {
        let mut state = self.state.clone();
        state.place(pl);
        let mut trace = self.trace.clone();
        trace.push(pl.idx as u8);
        let g = penalty(params, trace.len());
        let (eval, e) = eval(state.matrix());
        let h = eval.score(params);
        Self {
            state,
            trace,
            f: g + h,
            parent_f: self.f,
            estimate: e
        }
    }

    /// Returns `true` if this node is a terminal node (aka leaf node).
    fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Returns the rating value for this node. Lower is always better.
    fn rating(&self) -> i64 {
        if self.state.reached_goal() {
            // just use number-of-pieces as rating
            self.trace.len() as i64
        } else {
            self.f.saturating_add(self.parent_f)
        }
    }

    fn trace<'a>(&'a self) -> impl Iterator<Item = usize> + 'a {
        self.trace.iter().map(|&i| i as usize)
    }
}

impl PartialEq for Node {
    fn eq(&self, rhs: &Self) -> bool {
        self.f == rhs.f
    }
}

impl PartialOrd for Node {
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(rhs))
    }
}

impl Eq for Node {}

impl Ord for Node {
    fn cmp(&self, rhs: &Self) -> std::cmp::Ordering {
        self.f.cmp(&rhs.f).reverse()
    }
}
