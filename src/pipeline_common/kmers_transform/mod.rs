pub mod structs;

use crate::config::{
    BucketIndexType, SortingHashType, DEFAULT_PER_CPU_BUFFER_SIZE, FIRST_BUCKETS_COUNT,
    MINIMUM_LOG_DELTA_TIME, MINIMUM_RESPLIT_SIZE, RESPLIT_MINIMIZER_MASK, SECOND_BUCKETS_COUNT,
};
use crate::hashes::HashableSequence;
use crate::io::concurrent::temp_reads::creads_utils::CompressedReadsBucketHelper;
use crate::io::concurrent::temp_reads::extra_data::SequenceExtraData;
use crate::pipeline_common::kmers_transform::structs::{BucketProcessData, ProcessQueueItem};
use crate::pipeline_common::minimizer_bucketing::MinimizerBucketingExecutor;
use crate::pipeline_common::minimizer_bucketing::MinimizerBucketingExecutorFactory;
use crate::utils::compressed_read::CompressedRead;
use crate::utils::resource_counter::ResourceCounter;
use crossbeam::queue::{ArrayQueue, SegQueue};
use parallel_processor::buckets::concurrent::{BucketsThreadBuffer, BucketsThreadDispatcher};
use parallel_processor::buckets::readers::compressed_binary_reader::{
    CompressedBinaryReader, CompressedStreamDecoder,
};
use parallel_processor::buckets::readers::generic_binary_reader::ChunkDecoder;
use parallel_processor::buckets::readers::lock_free_binary_reader::{
    LockFreeBinaryReader, LockFreeStreamDecoder,
};
use parallel_processor::memory_fs::file::reader::FileReader;
use parallel_processor::memory_fs::{MemoryFs, RemoveFileMode};
use parallel_processor::phase_times_monitor::PHASES_TIMES_MONITOR;
use parking_lot::lock_api::MutexGuard;
use parking_lot::{Mutex, RwLock};
use std::cmp::min;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use structs::ReadRef;

pub struct ReadDispatchInfo<E: SequenceExtraData> {
    pub bucket: BucketIndexType,
    pub hash: SortingHashType,
    pub flags: u8,
    pub extra_data: E,
}

pub trait KmersTransformExecutorFactory: Sized + 'static + Sync + Send {
    type SequencesResplitterFactory: MinimizerBucketingExecutorFactory<
        ExtraData = Self::AssociatedExtraData,
    >;
    type GlobalExtraData<'a>: Send + Sync + 'a;
    type AssociatedExtraData: SequenceExtraData;
    type ExecutorType<'a>: KmersTransformExecutor<'a, Self>;

    #[allow(non_camel_case_types)]
    type FLAGS_COUNT: typenum::uint::Unsigned;

    fn new_resplitter<'a, 'b: 'a>(
        global_data: &'a Self::GlobalExtraData<'b>,
    ) -> <Self::SequencesResplitterFactory as MinimizerBucketingExecutorFactory>::ExecutorType<'a>;

    fn new<'a>(global_data: &Self::GlobalExtraData<'a>) -> Self::ExecutorType<'a>;
}

pub trait KmersTransformExecutor<'x, F: KmersTransformExecutorFactory> {
    fn preprocess_bucket<'y: 'x>(
        &mut self,
        global_data: &F::GlobalExtraData<'y>,
        flags: u8,
        input_extra_data: F::AssociatedExtraData,
        read: CompressedRead,
    ) -> ReadDispatchInfo<F::AssociatedExtraData>;

    fn maybe_swap_bucket<'y: 'x>(&mut self, global_data: &F::GlobalExtraData<'y>);

    fn process_group<'y: 'x>(
        &mut self,
        global_data: &F::GlobalExtraData<'y>,
        reader: LockFreeBinaryReader,
    );

    fn finalize<'y: 'x>(self, global_data: &F::GlobalExtraData<'y>);
}

pub struct KmersTransform<'a, F: KmersTransformExecutorFactory> {
    buckets_count: usize,
    buckets_total_size: u64,
    buffer_files_counter: Arc<ResourceCounter>,

    global_extra_data: F::GlobalExtraData<'a>,

    files_queue: ArrayQueue<PathBuf>,

    current_bucket: RwLock<Weak<BucketProcessData<CompressedStreamDecoder>>>,
    current_resplit_bucket: RwLock<Weak<BucketProcessData<LockFreeStreamDecoder>>>,

    process_queue: Arc<SegQueue<ProcessQueueItem>>,
    reprocess_queue: Arc<SegQueue<PathBuf>>,

    resplit_buckets_index: AtomicU32,

    last_info_log: Mutex<Instant>,
    _phantom: PhantomData<F>,
}

impl<'a, F: KmersTransformExecutorFactory> KmersTransform<'a, F> {
    pub fn new(
        file_inputs: Vec<PathBuf>,
        buckets_count: usize,
        extra_buffers_count: usize,
        threads_count: usize,
        global_extra_data: F::GlobalExtraData<'a>,
    ) -> Self {
        let files_queue = ArrayQueue::new(file_inputs.len());

        let mut buckets_total_size = 0;

        file_inputs.into_iter().for_each(|f| {
            buckets_total_size += MemoryFs::get_file_size(&f).unwrap_or(0);
            files_queue.push(f).unwrap()
        });

        Self {
            buckets_count,
            buckets_total_size: buckets_total_size as u64,
            buffer_files_counter: ResourceCounter::new(
                (SECOND_BUCKETS_COUNT*16 + extra_buffers_count) as u64,
            ),
            global_extra_data,
            files_queue,
            current_bucket: RwLock::new(Weak::new()),
            current_resplit_bucket: RwLock::new(Weak::new()),
            process_queue: Arc::new(SegQueue::new()),
            reprocess_queue: Arc::new(SegQueue::new()),
            resplit_buckets_index: AtomicU32::new(0),
            last_info_log: Mutex::new(Instant::now()),
            _phantom: Default::default(),
        }
    }

    fn do_logging(&self) {
        let mut last_info_log = match self.last_info_log.try_lock() {
            None => return,
            Some(x) => x,
        };
        if last_info_log.elapsed() > MINIMUM_LOG_DELTA_TIME {
            let monitor = PHASES_TIMES_MONITOR.read();

            let processed_count = self.buckets_count - self.files_queue.len();

            let eta = Duration::from_secs(
                (monitor.get_phase_timer().as_secs_f64() / (processed_count as f64)
                    * (self.files_queue.len() as f64)) as u64,
            );

            let est_tot = Duration::from_secs(
                (monitor.get_phase_timer().as_secs_f64() / (processed_count as f64)
                    * (self.buckets_count as f64)) as u64,
            );

            println!(
                "Processing bucket {} of {} {} phase eta: {:.0?} est.tot: {:.0?}",
                processed_count,
                self.buckets_count,
                monitor.get_formatted_counter_without_memory(),
                eta,
                est_tot
            );
            *last_info_log = Instant::now();
        }
    }

    fn get_current_bucket<
        FileType: ChunkDecoder,
        Allocator: Fn() -> Option<Arc<BucketProcessData<FileType>>>,
    >(
        current_bucket: &RwLock<Weak<BucketProcessData<FileType>>>,
        alloc_fn: Allocator,
    ) -> Option<Arc<BucketProcessData<FileType>>> {
        fn get_valid_bucket<FileType: ChunkDecoder>(
            bucket: &Weak<BucketProcessData<FileType>>,
        ) -> Option<Arc<BucketProcessData<FileType>>> {
            if let Some(bucket) = bucket.upgrade() {
                if !bucket.reader.is_finished() {
                    return Some(bucket);
                }
            }
            return None;
        }

        loop {
            let bucket = current_bucket.read();

            if let Some(bucket) = get_valid_bucket(&bucket) {
                return Some(bucket);
            }

            drop(bucket);
            let mut bucket = current_bucket.write();

            if let Some(bucket) = get_valid_bucket(&bucket) {
                return Some(bucket);
            }

            let new_bucket = alloc_fn()?;

            *bucket = Arc::downgrade(&new_bucket);

            return Some(new_bucket);
        }
    }

    fn read_bucket(
        &self,
        executor: &mut F::ExecutorType<'a>,
        bucket: &BucketProcessData<CompressedStreamDecoder>,
        buffer: &mut BucketsThreadBuffer,
    ) {
        let mut continue_read = true;

        if bucket.reader.is_finished() {
            return;
        }

        let mut cmp_reads = BucketsThreadDispatcher::new(&bucket.buckets, buffer);

        while continue_read {
            continue_read = bucket.reader.decode_bucket_items_parallel::<CompressedReadsBucketHelper<F::AssociatedExtraData, F::FLAGS_COUNT>, _>(Vec::new(), |(flags, read_extra_data, read)| {
                    if cfg!(not(feature = "kmerge-read-push-disable")) {
                        let preprocess_info = executor.preprocess_bucket(
                            &self.global_extra_data,
                            flags,
                            read_extra_data,
                            read,
                        );

                        cmp_reads.add_element(
                            preprocess_info.bucket,
                            &preprocess_info.extra_data,
                            &ReadRef::<_, F::FLAGS_COUNT> {
                                flags,
                                read,
                                _phantom: Default::default()
                            },
                        );
                    }
                },
            );
        }
    }

    fn resplit_buckets<'b>(
        &self,
        resplitter: &mut <F::SequencesResplitterFactory as MinimizerBucketingExecutorFactory>::ExecutorType<'b>,
        resplit_buffer: &mut BucketsThreadBuffer,
    ) -> bool {
        let mut did_resplit = false;
        let mut preproc_info = <F::SequencesResplitterFactory as MinimizerBucketingExecutorFactory>::PreprocessInfo::default();

        if let Some(resplit_bucket) = Self::get_current_bucket(&self.current_resplit_bucket, || {
            let path = self.reprocess_queue.pop()?;
            let file_name = path.file_name().unwrap().to_os_string();
            Some(Arc::new(
                BucketProcessData::<LockFreeStreamDecoder>::new_blocking(
                    path,
                    file_name.to_str().unwrap(),
                    self.process_queue.clone(),
                    self.buffer_files_counter.clone(),
                    true,
                ),
            ))
        }) {
            did_resplit = true;

            let mut thread_buckets =
                BucketsThreadDispatcher::new(&resplit_bucket.buckets, resplit_buffer);

            while resplit_bucket
                .reader
                .decode_bucket_items_parallel::<ReadRef<F::AssociatedExtraData, F::FLAGS_COUNT>, _>(
                    Vec::new(),
                    |(ReadRef { flags, read, .. }, extra)| {
                        resplitter.reprocess_sequence(flags, &extra, &mut preproc_info);
                        resplitter.process_sequence::<_, _, { RESPLIT_MINIMIZER_MASK }>(
                            &preproc_info,
                            read,
                            0..read.bases_count(),
                            |bucket, seq, flags, extra| {
                                thread_buckets.add_element(
                                    bucket % (SECOND_BUCKETS_COUNT as BucketIndexType),
                                    &extra,
                                    &ReadRef::<F::AssociatedExtraData, F::FLAGS_COUNT> {
                                        flags,
                                        read: seq,
                                        _phantom: PhantomData,
                                    },
                                );
                            },
                        );
                    },
                )
            {
                continue;
            }

            thread_buckets.finalize();
        }

        did_resplit
    }

    fn process_buffers(&self, executor: &mut F::ExecutorType<'a>, typical_sub_bucket_size: usize) {
        while let Some(ProcessQueueItem {
            ref path,
            ref can_resplit,
            ..
        }) = self.process_queue.pop()
        {
            executor.maybe_swap_bucket(&self.global_extra_data);

            let file_size = FileReader::open(path).unwrap().total_file_size();
            let is_outlier = file_size > typical_sub_bucket_size * SECOND_BUCKETS_COUNT;

            if !is_outlier
                || !*can_resplit
                || (file_size < MINIMUM_RESPLIT_SIZE)
                || cfg!(feature = "kmerge-read-resplit-disable")
            {
                let reader =
                    LockFreeBinaryReader::new(path, RemoveFileMode::Remove { remove_fs: true });
                if cfg!(not(feature = "kmerge-read-processing-disable")) {
                    executor.process_group(&self.global_extra_data, reader);
                }
            } else {
                println!(
                    "Resplitting bucket {} size: {} / {} [{}]",
                    path.display(),
                    file_size,
                    typical_sub_bucket_size * SECOND_BUCKETS_COUNT,
                    typical_sub_bucket_size
                );
                self.reprocess_queue.push(path.clone());
            }
        }
    }

    pub fn parallel_kmers_transform(&self, threads_count: usize) {
        let typical_sub_bucket_size =
            self.buckets_total_size as usize / (FIRST_BUCKETS_COUNT * SECOND_BUCKETS_COUNT);

        crossbeam::thread::scope(|s| {
            for _ in 0..min(self.buckets_count, threads_count) {
                s.builder()
                    .name("kmers-transform".to_string())
                    .spawn(|_| {
                        let mut executor = F::new(&self.global_extra_data);
                        let mut splitter = F::new_resplitter(&self.global_extra_data);
                        let mut local_buffer = BucketsThreadBuffer::new(
                            DEFAULT_PER_CPU_BUFFER_SIZE,
                            SECOND_BUCKETS_COUNT,
                        );

                        loop {
                            self.process_buffers(&mut executor, typical_sub_bucket_size);

                            if self.resplit_buckets(&mut splitter, &mut local_buffer) {
                                continue;
                            }

                            let bucket =
                                match Self::get_current_bucket(&self.current_bucket, || {
                                    let file = self.files_queue.pop()?;

                                    Some(Arc::new(BucketProcessData::new_blocking(
                                        file,
                                        &format!(
                                            "vec{}",
                                            self.resplit_buckets_index
                                                .fetch_add(1, Ordering::Relaxed)
                                        ),
                                        self.process_queue.clone(),
                                        self.buffer_files_counter.clone(),
                                        false,
                                    )))
                                }) {
                                    None => {
                                        if self.process_queue.is_empty()
                                            && self.reprocess_queue.is_empty()
                                        {
                                            break;
                                        } else {
                                            continue;
                                        }
                                    }
                                    Some(x) => x,
                                };

                            self.do_logging();

                            self.read_bucket(&mut executor, &bucket, &mut local_buffer);
                        }
                        executor.finalize(&self.global_extra_data);
                    })
                    .unwrap();
            }
        })
        .unwrap();
    }
}
