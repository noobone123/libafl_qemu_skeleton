use core::fmt::Debug;
use std::{fs, marker::PhantomData, ops::Range, path::PathBuf, process};

#[cfg(feature = "simplemgr")]
use libafl::events::SimpleEventManager;
#[cfg(not(feature = "simplemgr"))]
use libafl::events::{LlmpRestartingEventManager, MonitorTypedEventManager};
use libafl::{
    corpus::{Corpus, InMemoryOnDiskCorpus, OnDiskCorpus}, events::{ClientDescription, EventRestarter, NopEventManager}, executors::{Executor, ShadowExecutor}, feedback_and_fast, feedback_or, feedback_or_fast, feedbacks::{BoolValueFeedback, CrashFeedback, MaxMapFeedback, TimeFeedback, TimeoutFeedback}, fuzzer::{Evaluator, Fuzzer, StdFuzzer}, inputs::BytesInput, monitors::Monitor, mutators::{
        havoc_mutations, token_mutations::I2SRandReplace, tokens_mutations, StdMOptMutator,
        StdScheduledMutator, Tokens,
    }, observers::{CanTrack, HitcountsMapObserver, TimeObserver, VariableMapObserver}, schedulers::{
        powersched::PowerSchedule, IndexesLenTimeMinimizerScheduler, PowerQueueScheduler,
    }, stages::{
        calibrate::CalibrationStage, power::StdPowerMutationalStage, AflStatsStage, IfStage,
        ShadowTracingStage, StagesTuple, StdMutationalStage,
    }, state::{HasCorpus, StdState}, Error, HasMetadata
};
#[cfg(not(feature = "simplemgr"))]
use libafl_bolts::shmem::StdShMemProvider;
use libafl_bolts::{
    ownedref::OwnedMutSlice,
    rands::StdRand,
    tuples::{tuple_list, Merge, Prepend},
};
use libafl_qemu::{
    elf::EasyElf,
    modules::{
        calls::{CallTraceCollector, FullBacktraceCollector}, cmplog::CmpLogObserver, edges::EdgeCoverageFullVariant, utils::filters::{NopPageFilter, StdAddressFilter}, CallTracerModule, EdgeCoverageModule, EmulatorModule, EmulatorModuleTuple, SnapshotModule, StdEdgeCoverageModule
    },
    Emulator, GuestAddr, Qemu, QemuExecutor,
};
use libafl_targets::{edges_map_mut_ptr, EDGES_MAP_DEFAULT_SIZE, MAX_EDGES_FOUND};
use typed_builder::TypedBuilder;

use crate::{
    feedbacks::ignore_exit::IgnoreExitFeedback, harness::Harness, modules::{InputInjectorModule, RegisterResetModule}, options::FuzzerOptions
};

pub type ClientState =
    StdState<InMemoryOnDiskCorpus<BytesInput>, BytesInput, StdRand, OnDiskCorpus<BytesInput>>;

#[cfg(feature = "simplemgr")]
pub type ClientMgr<M> = SimpleEventManager<BytesInput, M, ClientState>;
#[cfg(not(feature = "simplemgr"))]
pub type ClientMgr<M> =
    MonitorTypedEventManager<LlmpRestartingEventManager<(), ClientState, StdShMemProvider>, M>;

#[derive(TypedBuilder)]
pub struct Instance<'a, M: Monitor> {
    options: &'a FuzzerOptions,
    /// The harness. We create it before forking, then `take()` it inside the client.
    mgr: ClientMgr<M>,
    client_description: ClientDescription,
    #[builder(default)]
    extra_tokens: Vec<String>,
    #[builder(default=PhantomData)]
    phantom: PhantomData<M>,
}

impl<M: Monitor> Instance<'_, M> {
    fn coverage_filter(&self, qemu: Qemu) -> Result<StdAddressFilter, Error> {
        /* Conversion is required on 32-bit targets, but not on 64-bit ones */
        if let Some(includes) = &self.options.include {
            #[cfg_attr(target_pointer_width = "64", allow(clippy::useless_conversion))]
            let rules = includes
                .iter()
                .map(|x| Range {
                    start: x.start.into(),
                    end: x.end.into(),
                })
                .collect::<Vec<Range<GuestAddr>>>();
            Ok(StdAddressFilter::allow_list(rules))
        } else if let Some(excludes) = &self.options.exclude {
            #[cfg_attr(target_pointer_width = "64", allow(clippy::useless_conversion))]
            let rules = excludes
                .iter()
                .map(|x| Range {
                    start: x.start.into(),
                    end: x.end.into(),
                })
                .collect::<Vec<Range<GuestAddr>>>();
            Ok(StdAddressFilter::deny_list(rules))
        } else {
            let mut elf_buffer = Vec::new();
            let elf = EasyElf::from_file(qemu.binary_path(), &mut elf_buffer)?;
            let range = elf
                .get_section(".text", qemu.load_addr())
                .ok_or_else(|| Error::key_not_found("Failed to find .text section"))?;
            Ok(StdAddressFilter::allow_list(vec![range]))
        }
    }

    #[expect(clippy::too_many_lines)]
    pub fn run<ET>(
        &mut self,
        args: Vec<String>,
        modules: ET,
        state: Option<ClientState>,
    ) -> Result<(), Error>
    where
        ET: EmulatorModuleTuple<BytesInput, ClientState> + Debug,
    {
        // Create an observation channel using the coverage map
        let mut edges_observer = unsafe {
            HitcountsMapObserver::new(VariableMapObserver::from_mut_slice(
                "edges",
                OwnedMutSlice::from_raw_parts_mut(edges_map_mut_ptr(), EDGES_MAP_DEFAULT_SIZE),
                &raw mut MAX_EDGES_FOUND,
            ))
            .track_indices()
        };

        /*
           Initialize the EmulatorModules and pass them into the Emulator
        */
        let edge_coverage_module = StdEdgeCoverageModule::builder()
            .map_observer(edges_observer.as_mut())
            .build()?;

        let reg_reset_module = RegisterResetModule::new();
        // // custom snapshot module and make `SnapshotModule` as its inner field is not supported and will cause a panic
        let snapshot_module = SnapshotModule::new();
        let input_injector_module = InputInjectorModule::new();

        // Be careful the order of the modules ...
        let modules = modules
            .prepend(edge_coverage_module)
            .prepend(input_injector_module)
            .prepend(reg_reset_module)
            .prepend(snapshot_module);

        /*
           Initialize the Emulator, Qemu (initialized in emulator) and Harness
        */
        log::info!("Qemu Parameters: {:?}", args);
        let mut emulator = Emulator::empty()
            .qemu_parameters(args)
            .modules(modules)
            .build()?;

        let qemu = emulator.qemu();
        let harness = Harness::init(qemu).expect("Error setting up harness.");

        /*
           Post-update the EmulatorModules after Qemu has been initialized
        */
        // update address filter after qemu has been initialized
        <EdgeCoverageModule<StdAddressFilter, NopPageFilter, EdgeCoverageFullVariant, false, 0> 
            as EmulatorModule<BytesInput, ClientState>>::update_address_filter(
                emulator.modules_mut()
                    .get_mut::<EdgeCoverageModule<StdAddressFilter, NopPageFilter, EdgeCoverageFullVariant, false, 0>>()
                    .expect("Could not find back the edge module"), 
                qemu,
                self.coverage_filter(qemu)?
        );

        // Save the current state of the registers
        emulator
            .modules_mut()
            .get_mut::<RegisterResetModule>()
            .expect("Could not find back the register reset module")
            .save(qemu);

        // Set the input address for the input injector module
        emulator
            .modules_mut()
            .get_mut::<InputInjectorModule>()
            .expect("Could not find back the input injector module")
            .set_input_addr(harness.input_addr);

        /*
         * Add Other Fuzzer Components
         */
        // Create an observation channel to keep track of the execution time
        let time_observer = TimeObserver::new("time");

        let map_feedback = MaxMapFeedback::new(&edges_observer);

        // If this input should not be ignored, `is_interesting` will return true
        let ignore_exit_feedback = IgnoreExitFeedback;

        let calibration = CalibrationStage::new(&map_feedback);

        let stats_stage = IfStage::new(
            |_, _, _, _| Ok(self.options.tui),
            tuple_list!(AflStatsStage::builder()
                .map_observer(&edges_observer)
                .stats_file(PathBuf::from("stats.txt"))
                .build()?),
        );

        // Feedback to rate the interestingness of an input
        // This one is composed by two Feedbacks in OR
        let mut feedback = feedback_or!(
            // New maximization map feedback linked to the edges observer and the feedback state
            feedback_and_fast!(
                map_feedback,
                ignore_exit_feedback
            ),
            // Time feedback, this one does not need a feedback state
            TimeFeedback::new(&time_observer)
        );

        // A feedback to choose if an input is a solution or not
        let mut objective = feedback_and_fast!(
            CrashFeedback::new(),
            MaxMapFeedback::new(&edges_observer));

        // // If not restarting, create a State from scratch
        let mut state = match state {
            Some(x) => x,
            None => {
                StdState::new(
                    // RNG
                    StdRand::new(),
                    // Corpus that will be evolved, we keep it in memory for performance
                    InMemoryOnDiskCorpus::no_meta(
                        self.options.queue_dir(self.client_description.clone()),
                    )?,
                    // Corpus in which we store solutions (crashes in this example),
                    // on disk so the user can get them after stopping the fuzzer
                    OnDiskCorpus::new(self.options.crashes_dir(self.client_description.clone()))?,
                    // States of the feedbacks.
                    // The feedbacks can report the data that should persist in the State.
                    &mut feedback,
                    // Same for objective feedbacks
                    &mut objective,
                )?
            }
        };

        // A minimization+queue policy to get testcasess from the corpus
        let scheduler = IndexesLenTimeMinimizerScheduler::new(
            &edges_observer,
            PowerQueueScheduler::new(&mut state, &edges_observer, PowerSchedule::fast()),
        );

        let observers = tuple_list!(edges_observer, time_observer);

        let mut tokens = Tokens::new();

        for token in &self.extra_tokens {
            let bytes = token.as_bytes().to_vec();
            let _ = tokens.add_token(&bytes);
        }

        if let Some(tokenfile) = &self.options.tokens {
            tokens.add_from_file(tokenfile)?;
        }

        state.add_metadata(tokens);

        harness.post_fork();
        
        // For current testing, the harness only needs to run once, so we do not need to reset the program state.
        let mut harness = |_emulator: &mut Emulator<_, _, _, _, _, _, _>,
                           _state: &mut _,
                           input: &BytesInput| harness.run(_emulator.qemu());

        // A fuzzer with feedbacks and a corpus scheduler
        let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

        if let Some(rerun_input) = &self.options.rerun_input {
            // TODO: We might want to support non-bytes inputs at some point?
            let bytes = fs::read(rerun_input)
                .unwrap_or_else(|_| panic!("Could not load file {rerun_input:?}"));
            let input = BytesInput::new(bytes);

            let mut executor = QemuExecutor::new(
                emulator,
                &mut harness,
                observers,
                &mut fuzzer,
                &mut state,
                &mut self.mgr,
                self.options.timeout,
            )?;

            executor
                .run_target(
                    &mut fuzzer,
                    &mut state,
                    &mut self.mgr,
                    &input,
                )
                .expect("Error running target");
            // We're done :)
            process::exit(0);
        }

        if self
            .options
            .is_cmplog_core(self.client_description.core_id())
        {
            // Create a QEMU in-process executor
            let executor = QemuExecutor::new(
                emulator,
                &mut harness,
                observers,
                &mut fuzzer,
                &mut state,
                &mut self.mgr,
                self.options.timeout,
            )?;

            // Create an observation channel using cmplog map
            let cmplog_observer = CmpLogObserver::new("cmplog", true);

            let mut executor = ShadowExecutor::new(executor, tuple_list!(cmplog_observer));

            let tracing = ShadowTracingStage::new(&mut executor);

            // Setup a randomic Input2State stage
            let i2s = StdMutationalStage::new(StdScheduledMutator::new(tuple_list!(
                I2SRandReplace::new()
            )));

            // Setup a MOPT mutator
            let mutator = StdMOptMutator::new(
                &mut state,
                havoc_mutations().merge(tokens_mutations()),
                7,
                5,
            )?;

            let power: StdPowerMutationalStage<_, _, BytesInput, _, _, _> =
                StdPowerMutationalStage::new(mutator);

            // The order of the stages matter!
            let mut stages = tuple_list!(calibration, tracing, i2s, power, stats_stage);

            self.fuzz(&mut state, &mut fuzzer, &mut executor, &mut stages)
        } else {
            // Create a QEMU in-process executor
            let mut executor = QemuExecutor::new(
                emulator,
                &mut harness,
                observers,
                &mut fuzzer,
                &mut state,
                &mut self.mgr,
                self.options.timeout,
            )?;

            // Setup an havoc mutator with a mutational stage
            let mutator = StdScheduledMutator::new(havoc_mutations().merge(tokens_mutations()));
            let mut stages = tuple_list!(StdMutationalStage::new(mutator));

            self.fuzz(&mut state, &mut fuzzer, &mut executor, &mut stages)
        }
    }

    fn fuzz<Z, E, ST>(
        &mut self,
        state: &mut ClientState,
        fuzzer: &mut Z,
        executor: &mut E,
        stages: &mut ST,
    ) -> Result<(), Error>
    where
        Z: Fuzzer<E, ClientMgr<M>, BytesInput, ClientState, ST>
        + Evaluator<E, ClientMgr<M>, BytesInput, ClientState>,
        ST: StagesTuple<E, ClientMgr<M>, ClientState, Z>,
    {
        let corpus_dirs = [self.options.input_dir()];

        if state.must_load_initial_inputs() {
            state
                .load_initial_inputs(fuzzer, executor, &mut self.mgr, &corpus_dirs)
                .unwrap_or_else(|_| {
                    println!("Failed to load initial corpus at {corpus_dirs:?}");
                    process::exit(0);
                });
            println!("We imported {} inputs from disk.", state.corpus().count());
        }

        if let Some(iters) = self.options.iterations {
            fuzzer.fuzz_loop_for(stages, executor, state, &mut self.mgr, iters)?;

            // It's important, that we store the state before restarting!
            // Else, the parent will not respawn a new child and quit.
            self.mgr.on_restart(state)?;
        } else {
            log::info!("Ready go into fuzzloop ...");
            fuzzer.fuzz_loop(stages, executor, state, &mut self.mgr)?;
        }

        Ok(())
    }
}
