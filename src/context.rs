use std::alloc::Layout;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};

use bitbybit::bitenum;

use crate::context::layout::{
    GcArrayHeader, GcArrayLayoutInfo, GcArrayTypeInfo, GcHeader, GcMarkBits, GcStateBits,
    GcTypeInfo, HeaderMetadata, TraceFuncPtr,
};
use crate::context::old::OldGenerationSpace;
use crate::context::young::{YoungAllocError, YoungGenerationSpace};
use crate::gcptr::Gc;
use crate::utils::AbortFailureGuard;
use crate::Collect;

mod alloc;
pub(crate) mod layout;
mod old;
mod young;

pub enum SingletonStatus {
    /// The singleton is thread-local.
    ///
    /// This is slower to resolve,
    /// but can be assumed to be unique
    /// within the confines of an individual thread.
    ///
    /// This implies the [`CollectorId`] is `!Send`
    ThreadLocal,
    /// The singleton is global.
    ///
    /// This is faster to resolve,
    /// and can further assume to be unique
    /// across the entire program.
    Global,
}

/// An opaque identifier for a specific garbage collector.
///
/// There is not necessarily a single global garbage collector.
/// There can be multiple ones as long as they have separate [`CollectorId`]s.
///
/// ## Safety
/// This type must be `#[repr(C)`] and its alignment must be at most eight bytes.
pub unsafe trait CollectorId: Copy + Debug + Eq + 'static {
    const SINGLETON: Option<SingletonStatus>;

    // TODO :This method is unsafe because of mutable aliasing
    // unsafe fn resolve_collector(&self) -> *mut GarbageCollector<Self>;

    unsafe fn summon_singleton() -> Option<Self>;
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CollectStageTracker {
    NotCollecting,
    Stage { current: CollectStage },
    FinishedStage { last_stage: CollectStage },
}

impl CollectStageTracker {
    #[inline]
    fn begin_stage(&mut self, expected_stage: Option<CollectStage>, new_stage: CollectStage) {
        assert_eq!(
            match expected_stage {
                Some(last_stage) => CollectStageTracker::FinishedStage { last_stage },
                None => CollectStageTracker::NotCollecting,
            },
            *self
        );
        *self = CollectStageTracker::Stage { current: new_stage };
    }

    #[inline]
    fn finish_stage(&mut self, stage: CollectStage) {
        assert_eq!(CollectStageTracker::Stage { current: stage }, *self);
        *self = CollectStageTracker::FinishedStage { last_stage: stage };
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum CollectStage {
    Mark,
    Sweep,
}

/// The state of a [GarbageCollector]
///
/// Seperated out to pass around as a separate reference.
/// This is important to avoid `&mut` from different sub-structures.
pub(crate) struct CollectorState<Id: CollectorId> {
    collector_id: Id,
    mark_bits_inverted: Cell<bool>,
}

struct GcRootBox<Id: CollectorId> {
    header: Cell<NonNull<GcHeader<Id>>>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct GenerationSizes {
    young_generation_size: usize,
    old_generation_size: usize,
}
impl GenerationSizes {
    const INITIAL_COLLECT_THRESHOLD: Self = GenerationSizes {
        young_generation_size: 12 * 1024,
        old_generation_size: 12 * 1204,
    };

    #[inline]
    pub fn meets_either_threshold(&self, threshold: GenerationSizes) -> bool {
        self.young_generation_size >= threshold.young_generation_size
            || self.old_generation_size >= threshold.old_generation_size
    }
}

pub struct GarbageCollector<Id: CollectorId> {
    state: CollectorState<Id>,
    young_generation: YoungGenerationSpace<Id>,
    old_generation: OldGenerationSpace<Id>,
    roots: RefCell<Vec<Weak<GcRootBox<Id>>>>,
    last_collect_size: Option<GenerationSizes>,
    collector_id: Id,
}
impl<Id: CollectorId> GarbageCollector<Id> {
    pub unsafe fn with_id(id: Id) -> Self {
        GarbageCollector {
            state: CollectorState {
                collector_id: id,
                mark_bits_inverted: Cell::new(false),
            },
            young_generation: YoungGenerationSpace::new(id),
            old_generation: OldGenerationSpace::new(id),
            roots: RefCell::new(Vec::new()),
            last_collect_size: None,
            collector_id: id,
        }
    }

    #[inline]
    pub fn id(&self) -> Id {
        self.collector_id
    }

    #[inline(always)]
    pub fn alloc<T: Collect<Id>>(&self, value: T) -> Gc<'_, T, Id> {
        self.alloc_with(|| value)
    }

    /// Allocate a GC object, initializng it with the specified closure.
    #[inline(always)]
    #[track_caller]
    pub fn alloc_with<T: Collect<Id>>(&self, func: impl FnOnce() -> T) -> Gc<'_, T, Id> {
        unsafe {
            let header = self.alloc_raw(&RegularAlloc {
                state: &self.state,
                type_info: GcTypeInfo::new::<T>(),
            });
            let initialization_guard = DestroyUninitValueGuard {
                header,
                old_generation: &self.old_generation,
            };
            let value_ptr = header.as_ref().regular_value_ptr().cast::<T>();
            value_ptr.as_ptr().write(func());
            header
                .as_ref()
                .update_state_bits(|state| state.with_value_initialized(true));
            initialization_guard.defuse(); // successful initialization;
            Gc::from_raw_ptr(value_ptr)
        }
    }

    #[inline]
    unsafe fn alloc_raw<T: RawAllocTarget<Id>>(&self, target: &T) -> NonNull<T::Header> {
        match self.young_generation.alloc_raw(target) {
            Ok(res) => res,
            Err(YoungAllocError::SizeExceedsLimit) => self.alloc_raw_fallback(target),
            Err(error @ YoungAllocError::OutOfMemory) => Self::oom(error),
        }
    }

    #[cold]
    unsafe fn alloc_raw_fallback<T: RawAllocTarget<Id>>(&self, target: &T) -> NonNull<T::Header> {
        self.old_generation
            .alloc_raw(target)
            .unwrap_or_else(|err| Self::oom(err))
    }

    #[cold]
    #[inline(never)]
    fn oom<E: Error>(error: E) -> ! {
        panic!("Fatal allocation error: {error}")
    }

    #[inline]
    pub fn root<'gc, T: Collect<Id>>(
        &'gc self,
        val: Gc<'gc, T, Id>,
    ) -> GcHandle<T::Collected<'static>, Id> {
        let mut roots = self.roots.borrow_mut();
        let root = Rc::new(GcRootBox {
            header: Cell::new(NonNull::from(val.header())),
        });
        roots.push(Rc::downgrade(&root));
        drop(roots); // drop refcell guard
        GcHandle {
            ptr: root,
            id: self.id(),
            marker: PhantomData,
        }
    }

    #[inline]
    pub fn collect(&mut self) {
        if self.needs_collection() {
            self.force_collect();
        }
    }

    #[cold]
    pub fn force_collect(&mut self) {
        // mark roots
        let mut context = CollectContext {
            garbage_collector: self,
            id: self.collector_id,
        };
        let failure_guard = AbortFailureGuard::new("GC failure to trace is fatal");
        let mut roots = self.roots.borrow_mut();
        roots.retain(|root| {
            match root.upgrade() {
                Some(root) => {
                    let new_header = unsafe { context.collect_gcheader(root.header.get()) };
                    root.header.set(new_header);
                    true // keep live root
                }
                None => false, // delete dead root
            }
        });
        drop(roots); // release guard
                     // tracing failure is fatal, but sweeping fatal is fine
        failure_guard.defuse();
        // now sweep
        unsafe {
            self.young_generation.sweep(&self.state);
            self.old_generation.sweep(&self.state);
        }
        // touch roots to verify validity
        #[cfg(debug_assertions)]
        for root in self.roots.get_mut().iter() {
            unsafe {
                assert!(!root
                    .upgrade()
                    .unwrap()
                    .header
                    .get()
                    .as_ref()
                    .state_bits
                    .get()
                    .forwarded());
            }
        }

        // invert meaning of the mark bits
        self.state
            .mark_bits_inverted
            .set(!self.state.mark_bits_inverted.get());
        // count size to trigger next gc
        self.last_collect_size = Some(self.current_size());
    }

    #[inline]
    fn current_size(&self) -> GenerationSizes {
        GenerationSizes {
            old_generation_size: self.old_generation.allocated_bytes(),
            young_generation_size: self.young_generation.allocated_bytes(),
        }
    }

    #[inline]
    fn threshold_size(&self) -> GenerationSizes {
        match self.last_collect_size {
            None => GenerationSizes::INITIAL_COLLECT_THRESHOLD,
            Some(last_sizes) => GenerationSizes {
                young_generation_size: last_sizes.young_generation_size * 2,
                old_generation_size: last_sizes.old_generation_size * 2,
            },
        }
    }

    #[inline]
    fn needs_collection(&self) -> bool {
        self.current_size()
            .meets_either_threshold(self.threshold_size())
    }
}

pub struct GcHandle<T: Collect<Id>, Id: CollectorId> {
    ptr: Rc<GcRootBox<Id>>,
    id: Id,
    marker: PhantomData<T>,
}
impl<T: Collect<Id>, Id: CollectorId> GcHandle<T, Id> {
    /// Resolve this handle into a [`Gc`] smart-pointer.
    ///
    /// ## Safety
    /// Even if this handle is dropped, the value will live until the next collection.
    /// This makes it valid for `'gc`.
    #[inline]
    pub fn resolve<'gc>(
        &self,
        collector: &'gc GarbageCollector<Id>,
    ) -> Gc<'gc, T::Collected<'gc>, Id> {
        assert_eq!(self.id, collector.id());
        // reload from GcRootBox in case pointer moved
        unsafe { Gc::from_raw_ptr(self.ptr.header.get().as_ref().regular_value_ptr().cast()) }
    }
}

unsafe trait RawAllocTarget<Id: CollectorId> {
    const ARRAY: bool;
    type Header: Sized;
    fn header_metadata(&self) -> HeaderMetadata<Id>;
    fn needs_drop(&self) -> bool;
    unsafe fn init_header(&self, header_ptr: NonNull<Self::Header>, base_header: GcHeader<Id>);
    fn overall_layout(&self) -> Layout;
    #[inline]
    fn init_state_bits(&self, gen: GenerationId) -> GcStateBits {
        GcStateBits::builder()
            .with_forwarded(false)
            .with_generation(gen)
            .with_array(Self::ARRAY)
            .with_raw_mark_bits(GcMarkBits::White.to_raw(self.collector_state()))
            .with_value_initialized(false)
            .build()
    }

    fn collector_state(&self) -> &'_ CollectorState<Id>;
}
struct RegularAlloc<'a, Id: CollectorId> {
    state: &'a CollectorState<Id>,
    type_info: &'static GcTypeInfo<Id>,
}
unsafe impl<Id: CollectorId> RawAllocTarget<Id> for RegularAlloc<'_, Id> {
    const ARRAY: bool = false;
    type Header = GcHeader<Id>;

    #[inline]
    fn header_metadata(&self) -> HeaderMetadata<Id> {
        HeaderMetadata {
            type_info: self.type_info,
        }
    }

    #[inline]
    fn needs_drop(&self) -> bool {
        self.type_info.drop_func.is_some()
    }

    #[inline]
    unsafe fn init_header(&self, header_ptr: NonNull<GcHeader<Id>>, base_header: GcHeader<Id>) {
        header_ptr.as_ptr().write(base_header)
    }

    #[inline]
    fn overall_layout(&self) -> Layout {
        unsafe {
            Layout::from_size_align_unchecked(
                self.type_info.layout.overall_layout().size(),
                GcHeader::<Id>::FIXED_ALIGNMENT,
            )
        }
    }

    #[inline]
    fn collector_state(&self) -> &'_ CollectorState<Id> {
        self.state
    }
}
struct ArrayAlloc<'a, Id: CollectorId> {
    type_info: &'static GcArrayTypeInfo<Id>,
    layout_info: GcArrayLayoutInfo<Id>,
    state: &'a CollectorState<Id>,
}
unsafe impl<Id: CollectorId> RawAllocTarget<Id> for ArrayAlloc<'_, Id> {
    const ARRAY: bool = true;
    type Header = GcArrayHeader<Id>;

    #[inline]
    fn header_metadata(&self) -> HeaderMetadata<Id> {
        HeaderMetadata {
            array_type_info: self.type_info,
        }
    }

    #[inline]
    fn needs_drop(&self) -> bool {
        self.type_info.element_type_info.drop_func.is_some()
    }

    #[inline]
    unsafe fn init_header(
        &self,
        header_ptr: NonNull<GcArrayHeader<Id>>,
        base_header: GcHeader<Id>,
    ) {
        header_ptr.as_ptr().write(GcArrayHeader {
            main_header: base_header,
            len_elements: self.layout_info.len_elements(),
        })
    }

    #[inline]
    fn overall_layout(&self) -> Layout {
        self.layout_info.overall_layout()
    }

    #[inline]
    fn collector_state(&self) -> &'_ CollectorState<Id> {
        self.state
    }
}

#[derive(Debug, Eq, PartialEq)]
#[bitenum(u1, exhaustive = true)]
enum GenerationId {
    Young = 0,
    Old = 1,
}

pub struct CollectContext<'newgc, Id: CollectorId> {
    id: Id,
    garbage_collector: &'newgc GarbageCollector<Id>,
}
impl<'newgc, Id: CollectorId> CollectContext<'newgc, Id> {
    #[inline]
    pub fn id(&self) -> Id {
        self.id
    }

    #[inline]
    pub unsafe fn trace_gc_ptr_mut<T: Collect<Id>>(&mut self, target: NonNull<Gc<'_, T, Id>>) {
        let target = target.as_ptr();
        target
            .cast::<Gc<'newgc, T::Collected<'newgc>, Id>>()
            .write(self.collect_gc_ptr(target.read()));
    }

    #[inline]
    unsafe fn collect_gc_ptr<'gc, T: Collect<Id>>(
        &mut self,
        target: Gc<'gc, T, Id>,
    ) -> Gc<'newgc, T::Collected<'newgc>, Id> {
        Gc::from_raw_ptr(
            self.collect_gcheader(NonNull::from(target.header()))
                .as_ref()
                .regular_value_ptr()
                .cast(),
        )
    }

    #[cfg_attr(not(debug_assertions), inline)]
    #[must_use]
    unsafe fn collect_gcheader(&mut self, header: NonNull<GcHeader<Id>>) -> NonNull<GcHeader<Id>> {
        let mark_bits: GcMarkBits;
        {
            let header = header.as_ref();
            assert_eq!(header.collector_id, self.id, "Mismatched collector ids");
            debug_assert!(
                !header.state_bits.get().array(),
                "Incorrectly marked as an array"
            );
            if header.state_bits.get().forwarded() {
                debug_assert_eq!(header.state_bits.get().generation(), GenerationId::Young);
                debug_assert_eq!(
                    header
                        .state_bits
                        .get()
                        .raw_mark_bits()
                        .resolve(&self.garbage_collector.state),
                    GcMarkBits::Black
                );
                return header.metadata.forward_ptr;
            }
            mark_bits = header
                .state_bits
                .get()
                .raw_mark_bits()
                .resolve(&self.garbage_collector.state);
        }
        match mark_bits {
            GcMarkBits::White => self.fallback_collect_gc_header(header),
            GcMarkBits::Black => header,
        }
    }

    #[cold]
    unsafe fn fallback_collect_gc_header(
        &mut self,
        header_ptr: NonNull<GcHeader<Id>>,
    ) -> NonNull<GcHeader<Id>> {
        let type_info: &'static GcTypeInfo<Id>;
        let array = header_ptr.as_ref().state_bits.get().array();
        debug_assert!(
            header_ptr.as_ref().state_bits.get().value_initialized(),
            "Traced value must be initialized: {:?}",
            header_ptr.as_ref()
        );
        let prev_generation: GenerationId;
        {
            let header = header_ptr.as_ref();
            debug_assert_eq!(
                header
                    .state_bits
                    .get()
                    .raw_mark_bits()
                    .resolve(&self.garbage_collector.state),
                GcMarkBits::White
            );
            // mark as black
            header.update_state_bits(|state_bits| {
                state_bits
                    .with_raw_mark_bits(GcMarkBits::Black.to_raw(&self.garbage_collector.state))
            });
            prev_generation = header.state_bits.get().generation();
            type_info = header.metadata.type_info;
        }
        let forwarded_ptr = match prev_generation {
            GenerationId::Young => {
                let array_value_size: Option<usize>;
                // reallocate in oldgen
                let copied_ptr = if array {
                    let array_type_info = type_info.assume_array_info();
                    debug_assert!(std::ptr::eq(
                        array_type_info,
                        header_ptr.as_ref().metadata.array_type_info
                    ));
                    let array_layout = GcArrayLayoutInfo::new_unchecked(
                        array_type_info.element_type_info.layout.value_layout(),
                        header_ptr.cast::<GcArrayHeader<Id>>().as_ref().len_elements,
                    );
                    array_value_size = Some(array_layout.value_layout().size());
                    self.garbage_collector
                        .old_generation
                        .alloc_raw(&ArrayAlloc {
                            layout_info: array_layout,
                            type_info: array_type_info,
                            state: &self.garbage_collector.state,
                        })
                        .map(NonNull::cast::<GcHeader<Id>>)
                } else {
                    array_value_size = None;
                    self.garbage_collector
                        .old_generation
                        .alloc_raw(&RegularAlloc {
                            type_info,
                            state: &self.garbage_collector.state,
                        })
                }
                .unwrap_or_else(|_| {
                    // TODO: This panic is fatal, will cause an abort
                    panic!("Oldgen alloc failure")
                });
                copied_ptr
                    .as_ref()
                    .state_bits
                    .set(header_ptr.as_ref().state_bits.get());
                copied_ptr.as_ref().update_state_bits(|bits| {
                    debug_assert!(!bits.forwarded());
                    bits.with_generation(GenerationId::Old)
                        .with_value_initialized(true)
                });
                header_ptr
                    .as_ref()
                    .update_state_bits(|bits| bits.with_forwarded(true));
                (&mut *header_ptr.as_ptr()).metadata.forward_ptr = copied_ptr.cast();
                // determine if drop is needed from header_ptr, avoiding an indirection to type_info
                let needs_drop = header_ptr.as_ref().alloc_info.nontrivial_drop_index < u32::MAX;
                debug_assert_eq!(needs_drop, type_info.drop_func.is_some());
                if needs_drop {
                    self.garbage_collector
                        .young_generation
                        .remove_destruction_queue(header_ptr, &self.garbage_collector.state);
                }
                // NOTE: Copy uninitialized bytes is safe here, as long as they are not read in dest
                if array {
                    copied_ptr
                        .cast::<GcArrayHeader<Id>>()
                        .as_ref()
                        .array_value_ptr()
                        .cast::<u8>()
                        .as_ptr()
                        .copy_from_nonoverlapping(
                            header_ptr
                                .cast::<GcArrayHeader<Id>>()
                                .as_ref()
                                .array_value_ptr()
                                .as_ptr(),
                            array_value_size.unwrap(),
                        )
                } else {
                    copied_ptr
                        .as_ref()
                        .regular_value_ptr()
                        .cast::<u8>()
                        .as_ptr()
                        .copy_from_nonoverlapping(
                            header_ptr
                                .as_ref()
                                .regular_value_ptr()
                                .cast::<u8>()
                                .as_ptr(),
                            type_info.layout.value_layout().size(),
                        );
                }
                copied_ptr
            }
            GenerationId::Old => header_ptr, // no copying needed for oldgen
        };
        /*
         * finally, trace the value
         * this needs to come after forwarding and switching the mark bit
         * so we can properly update self-referential pointers
         */
        if let Some(trace_func) = type_info.trace_func {
            /*
             * NOTE: Cannot have aliasing &mut header references during this recursion
             * The parameters to maybe_grow are completely arbitrary right now.
             */
            #[cfg(not(miri))]
            stacker::maybe_grow(
                4096,       // 4KB
                128 * 1024, // 128KB
                || self.trace_children(forwarded_ptr, trace_func),
            );
            #[cfg(miri)]
            self.trace_children(forwarded_ptr, trace_func);
        }
        forwarded_ptr
    }

    #[inline]
    unsafe fn trace_children(
        &mut self,
        header: NonNull<GcHeader<Id>>,
        trace_func: TraceFuncPtr<Id>,
    ) {
        debug_assert!(
            !header.as_ref().state_bits.get().forwarded(),
            "Cannot be forwarded"
        );
        if header.as_ref().state_bits.get().array() {
            self.trace_children_array(header.cast(), trace_func);
        } else {
            trace_func(header.as_ref().regular_value_ptr().cast(), self);
        }
    }

    unsafe fn trace_children_array(
        &mut self,
        header: NonNull<GcArrayHeader<Id>>,
        trace_func: TraceFuncPtr<Id>,
    ) {
        let type_info = header.as_ref().main_header.metadata.type_info;
        debug_assert_eq!(type_info.trace_func, Some(trace_func));
        let array_header = header.cast::<GcArrayHeader<Id>>();
        for element in array_header.as_ref().iter_elements() {
            trace_func(element.cast::<()>(), self);
        }
    }
}

/// A RAII guard to destroy an uninitialized GC allocation.
///
/// Must explicitly call `defuse` after a successful initialization.
#[must_use]
struct DestroyUninitValueGuard<'a, Id: CollectorId> {
    header: NonNull<GcHeader<Id>>,
    old_generation: &'a OldGenerationSpace<Id>,
}
impl<'a, Id: CollectorId> DestroyUninitValueGuard<'a, Id> {
    #[inline]
    pub fn defuse(self) {
        debug_assert!(
            unsafe { self.header.as_ref().state_bits.get().value_initialized() },
            "Value not initialized"
        );
        std::mem::forget(self);
    }
}
impl<Id: CollectorId> Drop for DestroyUninitValueGuard<'_, Id> {
    #[cold]
    fn drop(&mut self) {
        // should only be called on failure
        unsafe {
            assert!(
                !self.header.as_ref().state_bits.get().value_initialized(),
                "Value successfully initialized but guard not defused"
            );
            match self.header.as_ref().state_bits.get().generation() {
                GenerationId::Old => {
                    // old-gen needs an explicit free
                    self.old_generation.destroy_uninit_object(self.header);
                }
                GenerationId::Young => {
                    // In young-gen, marking uninitialized is sufficient
                    // it will be automatically freed next sweep
                }
            }
        }
    }
}
