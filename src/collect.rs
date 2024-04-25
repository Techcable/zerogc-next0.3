//! Defines the [`Collect`] trait and implements it for several types

pub mod macros;
mod collections;
mod primitives;

use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use crate::CollectorId;
use crate::context::CollectContext;

pub unsafe trait Collect<Id: CollectorId> {
    type Collected<'newgc>: Collect<Id>;
    const NEEDS_COLLECT: bool;

    unsafe fn collect_inplace(
        target: NonNull<Self>,
        context: &mut CollectContext<'_, Id>
    );
}

pub unsafe trait NullCollect<Id: CollectorId>: Collect<Id> {}

//
// macros
//