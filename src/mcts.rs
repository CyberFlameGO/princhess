//! This is a library for Monte Carlo tree search.
//!
//! It is still under development and the documentation isn't good. However, the following example may be helpful:
//!
//! ```
//! use mcts::*;
//! use mcts::tree_policy::*;
//! use mcts::transposition_table::*;
//!
//! // A really simple game. There's one player and one number. In each move the player can
//! // increase or decrease the number. The player's score is the number.
//! // The game ends when the number reaches 100.
//! //
//! // The best strategy is to increase the number at every step.
//!
//! #[derive(Clone, Debug, PartialEq)]
//! struct CountingGame(i64);
//!
//! #[derive(Clone, Debug, PartialEq)]
//! enum Move {
//!     Add, Sub
//! }
//!
//! impl GameState for CountingGame {
//!     type Move = Move;
//!     type Player = ();
//!     type MoveList = Vec<Move>;
//!
//!     fn current_player(&self) -> Self::Player {
//!         ()
//!     }
//!     fn available_moves(&self) -> Vec<Move> {
//!         let x = self.0;
//!         if x == 100 {
//!             vec![]
//!         } else {
//!             vec![Move::Add, Move::Sub]
//!         }
//!     }
//!     fn make_move(&mut self, mov: &Self::Move) {
//!         match *mov {
//!             Move::Add => self.0 += 1,
//!             Move::Sub => self.0 -= 1,
//!         }
//!     }
//! }
//!
//! impl TranspositionHash for CountingGame {
//!     fn hash(&self) -> u64 {
//!         self.0 as u64
//!     }
//! }
//!
//! struct MyEvaluator;
//!
//! impl Evaluator<MyMCTS> for MyEvaluator {
//!     type StateEvaluation = i64;
//!
//!     fn evaluate_new_state(&self, state: &CountingGame, moves: &Vec<Move>,
//!         _: Option<SearchHandle<MyMCTS>>)
//!         -> (Vec<()>, i64) {
//!         (vec![(); moves.len()], state.0)
//!     }
//!     fn interpret_evaluation_for_player(&self, evaln: &i64, _player: &()) -> i64 {
//!         *evaln
//!     }
//!     fn evaluate_existing_state(&self, _: &CountingGame,  evaln: &i64, _: SearchHandle<MyMCTS>) -> i64 {
//!         *evaln
//!     }
//! }
//!
//! #[derive(Default)]
//! struct MyMCTS;
//!
//! impl MCTS for MyMCTS {
//!     type State = CountingGame;
//!     type Eval = MyEvaluator;
//!     type NodeData = ();
//!     type ExtraThreadData = ();
//!     type TreePolicy = UCTPolicy;
//!     type TranspositionTable = ApproxTable<Self>;
//!     type PlayoutData = ();
//!
//!     fn cycle_behaviour(&self) -> CycleBehaviour<Self> {
//!         CycleBehaviour::UseThisEvalWhenCycleDetected(0)
//!     }
//! }
//!
//! let game = CountingGame(0);
//! let mut mcts = MCTSManager::new(game, MyMCTS, MyEvaluator, UCTPolicy::new(0.5),
//!     ApproxTable::new(1024));
//! mcts.playout_n_parallel(10000, 4); // 10000 playouts, 4 search threads
//! mcts.tree().debug_moves();
//! assert_eq!(mcts.best_move().unwrap(), Move::Add);
//! assert_eq!(mcts.principal_variation(50),
//!     vec![Move::Add; 50]);
//! assert_eq!(mcts.principal_variation_states(5),
//!     vec![
//!         CountingGame(0),
//!         CountingGame(1),
//!         CountingGame(2),
//!         CountingGame(3),
//!         CountingGame(4),
//!         CountingGame(5)]);
//! ```
use arena::ArenaAllocator;
pub use search_tree::*;
use transposition_table::*;
use tree_policy::*;

use atomics::*;
use search::SCALE;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

pub trait MCTS: Sized + Sync {
    type State: GameState + Sync;
    type Eval: Evaluator<Self>;
    type TreePolicy: TreePolicy<Self>;
    type NodeData: Default + Sync + Send;
    type TranspositionTable: TranspositionTable<Self>;
    type ExtraThreadData;
    type PlayoutData: Default;

    /// Virtual loss subtracted from a node's evaluation when a search thread chooses it in a playout,
    /// then added back when the playout is complete.
    /// Used to reduce contention between threads. Defaults to 0.
    fn virtual_loss(&self) -> i64 {
        0
    }
    /// The number of times a node must be visited before expanding its children.
    /// Defaults to 1.
    /// It only makes sense to use a value other than 1 if your evaluation can change on successive calls.
    fn visits_before_expansion(&self) -> u64 {
        1
    }
    /// Maximum number of nodes beyond which calling `playout` will do nothing. Defaults to `std::usize::MAX`.
    fn node_limit(&self) -> usize {
        std::usize::MAX
    }
    /// Rule for selecting the best move once the search is over. Defaults to choosing the child with the most visits.
    fn select_child_after_search<'a>(
        &self,
        children: &[MoveInfoHandle<'a, Self>],
    ) -> MoveInfoHandle<'a, Self> {
        *children
            .into_iter()
            .max_by_key(|child| child.visits())
            .unwrap()
    }
    /// `playout` panics when this length is exceeded. Defaults to one million.
    fn max_playout_length(&self) -> usize {
        1_000_000
    }
    /// Called when a playout is complete. The default implementation does nothing.
    fn on_backpropagation(&self, _evaln: &StateEvaluation<Self>, _handle: SearchHandle<Self>) {}
    fn cycle_behaviour(&self) -> CycleBehaviour<Self> {
        if std::mem::size_of::<Self::TranspositionTable>() == 0 {
            CycleBehaviour::Ignore
        } else {
            CycleBehaviour::PanicWhenCycleDetected
        }
    }
    /// Called when a child node is selected in a playout. The default implementation does nothing.
    fn on_choice_made<'a, 'b>(
        &self,
        _data: &mut Self::PlayoutData,
        _state: &Self::State,
        _moves: Moves<'a, Self>,
        _choice: MoveInfoHandle<'a, Self>,
        _handle: SearchHandle<'a, 'b, Self>,
    ) {
    }
    /// Called before the tree policy is run. If it returns `Some(x)`, the tree policy is ignored
    /// and `x` is used instead. The default implementation returns `None`.
    fn override_policy<'a>(
        &self,
        _data: &Self::PlayoutData,
        _state: &Self::State,
        _moves: Moves<'a, Self>,
    ) -> Option<MoveInfoHandle<'a, Self>> {
        None
    }
}

pub struct ThreadData<'a, Spec: MCTS> {
    pub policy_data: TreePolicyThreadData<Spec>,
    pub extra_data: Spec::ExtraThreadData,
    pub allocator: ArenaAllocator<'a>,
}

impl<'a, Spec: MCTS> ThreadData<'a, Spec>
where
    TreePolicyThreadData<Spec>: Default,
    Spec::ExtraThreadData: Default,
{
    fn create(tree: &'a SearchTree<Spec>) -> Self {
        Self {
            policy_data: Default::default(),
            extra_data: Default::default(),
            allocator: tree.arena().allocator(),
        }
    }
}

pub type MoveEvaluation<Spec> = <<Spec as MCTS>::TreePolicy as TreePolicy<Spec>>::MoveEvaluation;
pub type StateEvaluation<Spec> = <<Spec as MCTS>::Eval as Evaluator<Spec>>::StateEvaluation;
pub type Move<Spec> = <<Spec as MCTS>::State as GameState>::Move;
pub type MoveList<Spec> = <<Spec as MCTS>::State as GameState>::MoveList;
pub type Player<Spec> = <<Spec as MCTS>::State as GameState>::Player;
pub type TreePolicyThreadData<Spec> =
    <<Spec as MCTS>::TreePolicy as TreePolicy<Spec>>::ThreadLocalData;

pub trait GameState: Clone {
    type Move: Sync + Send + Clone;
    type Player: Sync;
    type MoveList: std::iter::IntoIterator<Item = Self::Move>;

    fn current_player(&self) -> Self::Player;
    fn available_moves(&self) -> Self::MoveList;
    fn make_move(&mut self, mov: &Self::Move);
}

pub trait Evaluator<Spec: MCTS>: Sync {
    type StateEvaluation: Sync + Send + Copy;

    fn evaluate_new_state(
        &self,
        state: &Spec::State,
        moves: &MoveList<Spec>,
    ) -> (Vec<MoveEvaluation<Spec>>, Self::StateEvaluation);

    fn evaluate_existing_state(
        &self,
        state: &Spec::State,
        existing_evaln: &Self::StateEvaluation,
        handle: SearchHandle<Spec>,
    ) -> Self::StateEvaluation;

    fn interpret_evaluation_for_player(
        &self,
        evaluation: &Self::StateEvaluation,
        player: &Player<Spec>,
    ) -> i64;
}

pub struct MCTSManager<Spec: MCTS> {
    search_tree: SearchTree<Spec>,
}

impl<Spec: MCTS> MCTSManager<Spec>
where
    TreePolicyThreadData<Spec>: Default,
    Spec::ExtraThreadData: Default,
{
    pub fn new(
        state: Spec::State,
        manager: Spec,
        eval: Spec::Eval,
        tree_policy: Spec::TreePolicy,
        table: Spec::TranspositionTable,
        prev_table: PreviousTable<Spec>,
    ) -> Self {
        let search_tree = SearchTree::new(state, manager, tree_policy, eval, table, prev_table);
        Self { search_tree }
    }

    pub fn playout_until<Predicate: FnMut() -> bool>(&mut self, mut pred: Predicate) {
        let mut tld = ThreadData::create(self.tree());
        while !pred() {
            self.tree().playout(&mut tld);
        }
    }
    pub fn playout_n(&mut self, n: u64) {
        let mut tld = ThreadData::create(self.tree());
        for _ in 0..n {
            self.tree().playout(&mut tld);
        }
    }
    unsafe fn spawn_worker_thread(&self, stop_signal: Arc<AtomicBool>) -> JoinHandle<()> {
        let search_tree = &self.search_tree;
        crossbeam::spawn_unsafe(move || {
            let mut tld = ThreadData::create(search_tree);
            loop {
                if stop_signal.load(Ordering::SeqCst) {
                    break;
                }
                if !search_tree.playout(&mut tld) {
                    break;
                }
            }
        })
    }
    pub fn playout_parallel_async<'a>(&'a mut self, num_threads: usize) -> AsyncSearch<'a, Spec> {
        assert!(num_threads != 0);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let threads = (0..num_threads)
            .map(|_| {
                let stop_signal = stop_signal.clone();
                unsafe { self.spawn_worker_thread(stop_signal) }
            })
            .collect();
        AsyncSearch {
            manager: self,
            stop_signal,
            threads,
        }
    }
    pub fn into_playout_parallel_async(self, num_threads: usize) -> AsyncSearchOwned<Spec> {
        assert!(num_threads != 0);
        let self_box = Box::new(self);
        let stop_signal = Arc::new(AtomicBool::new(false));
        let threads = (0..num_threads)
            .map(|_| {
                let stop_signal = stop_signal.clone();
                unsafe { self_box.spawn_worker_thread(stop_signal) }
            })
            .collect();
        AsyncSearchOwned {
            manager: Some(self_box),
            stop_signal,
            threads,
        }
    }
    pub fn playout_parallel_for(&mut self, duration: Duration, num_threads: usize) {
        let search = self.playout_parallel_async(num_threads);
        std::thread::sleep(duration);
        search.halt();
    }
    pub fn playout_n_parallel(&mut self, n: u32, num_threads: usize) {
        if n == 0 {
            return;
        }
        assert!(num_threads != 0);
        let counter = AtomicIsize::new(n as isize);
        let search_tree = &self.search_tree;
        crossbeam::scope(|scope| {
            for _ in 0..num_threads {
                scope.spawn(|| {
                    let mut tld = ThreadData::create(search_tree);
                    loop {
                        let count = counter.fetch_sub(1, Ordering::SeqCst);
                        if count <= 0 {
                            break;
                        }
                        search_tree.playout(&mut tld);
                    }
                });
            }
        });
    }
    pub fn principal_variation_info(&self, num_moves: usize) -> Vec<MoveInfoHandle<Spec>> {
        self.search_tree.principal_variation(num_moves)
    }
    pub fn principal_variation(&self, num_moves: usize) -> Vec<Move<Spec>> {
        self.search_tree
            .principal_variation(num_moves)
            .into_iter()
            .map(|x| x.get_move())
            .map(|x| x.clone())
            .collect()
    }
    pub fn principal_variation_states(&self, num_moves: usize) -> Vec<Spec::State> {
        let moves = self.principal_variation(num_moves);
        let mut states = vec![self.search_tree.root_state().clone()];
        for mov in moves {
            let mut state = states[states.len() - 1].clone();
            state.make_move(&mov);
            states.push(state);
        }
        states
    }

    pub fn tree(&self) -> &SearchTree<Spec> {
        &self.search_tree
    }

    pub fn table(self) -> PreviousTable<Spec> {
        self.search_tree.table()
    }

    pub fn best_move(&self) -> Option<Move<Spec>> {
        self.principal_variation(1).get(0).map(|x| x.clone())
    }

    pub fn eval_in_cp(&self) -> i64 {
        (100.0
            * (1.5
                * self
                    .principal_variation_info(1)
                    .get(0)
                    .map(|x| (x.sum_rewards() / x.visits() as i64) as f32 / SCALE)
                    .unwrap_or(0.0))
            .tan()) as i64
    }

    pub fn perf_test<F>(&mut self, num_threads: usize, mut f: F)
    where
        F: FnMut(usize),
    {
        let search = self.playout_parallel_async(num_threads);
        for _ in 0..10 {
            let n1 = search.manager.search_tree.num_nodes();
            std::thread::sleep(Duration::from_secs(1));
            let n2 = search.manager.search_tree.num_nodes();
            let diff = if n2 > n1 { n2 - n1 } else { 0 };
            f(diff);
        }
    }
    pub fn perf_test_to_stderr(&mut self, num_threads: usize) {
        let mut running_total = 0;
        self.perf_test(num_threads, |x| {
            running_total += x;
            eprintln!("{} nodes/sec", thousands_separate(x));
        });
        eprintln!("{} nodes total", thousands_separate(running_total));
    }
}

// https://stackoverflow.com/questions/26998485/rust-print-format-number-with-thousand-separator
pub fn thousands_separate(x: usize) -> String {
    let s = format!("{}", x);
    let bytes: Vec<_> = s.bytes().rev().collect();
    let chunks: Vec<_> = bytes
        .chunks(3)
        .map(|chunk| String::from_utf8(chunk.to_vec()).unwrap())
        .collect();
    let result: Vec<_> = chunks.join(",").bytes().rev().collect();
    String::from_utf8(result).unwrap()
}

#[must_use]
pub struct AsyncSearch<'a, Spec: 'a + MCTS> {
    manager: &'a mut MCTSManager<Spec>,
    stop_signal: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl<'a, Spec: MCTS> AsyncSearch<'a, Spec> {
    pub fn halt(self) {}
    pub fn num_threads(&self) -> usize {
        self.threads.len()
    }
}

impl<'a, Spec: MCTS> Drop for AsyncSearch<'a, Spec> {
    fn drop(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        drain_join_unwrap(&mut self.threads);
    }
}

#[must_use]
pub struct AsyncSearchOwned<Spec: MCTS> {
    manager: Option<Box<MCTSManager<Spec>>>,
    stop_signal: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl<Spec: MCTS> AsyncSearchOwned<Spec> {
    fn stop_threads(&mut self) {
        self.stop_signal.store(true, Ordering::SeqCst);
        drain_join_unwrap(&mut self.threads);
    }
    pub fn halt(mut self) -> MCTSManager<Spec> {
        self.stop_threads();
        *self.manager.take().unwrap()
    }
    pub fn num_threads(&self) -> usize {
        self.threads.len()
    }
}

impl<Spec: MCTS> Drop for AsyncSearchOwned<Spec> {
    fn drop(&mut self) {
        self.stop_threads();
    }
}

impl<Spec: MCTS> From<MCTSManager<Spec>> for AsyncSearchOwned<Spec> {
    /// An `MCTSManager` is an `AsyncSearchOwned` with zero threads searching.
    fn from(m: MCTSManager<Spec>) -> Self {
        Self {
            manager: Some(Box::new(m)),
            stop_signal: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        }
    }
}

fn drain_join_unwrap(threads: &mut Vec<JoinHandle<()>>) {
    let join_results: Vec<_> = threads.drain(..).map(|x| x.join()).collect();
    for x in join_results {
        x.unwrap();
    }
}

pub enum CycleBehaviour<Spec: MCTS> {
    Ignore,
    UseCurrentEvalWhenCycleDetected,
    PanicWhenCycleDetected,
    UseThisEvalWhenCycleDetected(StateEvaluation<Spec>),
}
