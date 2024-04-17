//! This crate provides a cross-platform library and binary for translating addresses into
//! function names, file names and line numbers. Given an address in an executable or an
//! offset in a section of a relocatable object, it uses the debugging information to
//! figure out which file name and line number are associated with it.
//!
//! When used as a library, files must first be loaded using the
//! [`object`](https://github.com/gimli-rs/object) crate.
//! A context can then be created with [`Context::new`](./struct.Context.html#method.new).
//! The context caches some of the parsed information so that multiple lookups are
//! efficient.
//! Location information is obtained with
//! [`Context::find_location`](./struct.Context.html#method.find_location) or
//! [`Context::find_location_range`](./struct.Context.html#method.find_location_range).
//! Function information is obtained with
//! [`Context::find_frames`](./struct.Context.html#method.find_frames), which returns
//! a frame for each inline function. Each frame contains both name and location.
//!
//! The crate has an example CLI wrapper around the library which provides some of
//! the functionality of the `addr2line` command line tool distributed with [GNU
//! binutils](https://www.gnu.org/software/binutils/).
//!
//! Currently this library only provides information from the DWARF debugging information,
//! which is parsed using [`gimli`](https://github.com/gimli-rs/gimli).  The example CLI
//! wrapper also uses symbol table information provided by the `object` crate.
#![deny(missing_docs)]
#![no_std]

#[cfg(feature = "std")]
extern crate std;

#[allow(unused_imports)]
#[macro_use]
extern crate alloc;

#[cfg(feature = "fallible-iterator")]
pub extern crate fallible_iterator;
pub extern crate gimli;
#[cfg(feature = "object")]
pub extern crate object;

use alloc::borrow::Cow;
#[cfg(feature = "object")]
use alloc::rc::Rc;
use alloc::sync::Arc;

use core::ops::ControlFlow;
use core::u64;

use crate::function::{Function, Functions, InlinedFunction, LazyFunctions};
use crate::line::{LazyLines, LineLocationRangeIter, Lines};
use crate::lookup::{LoopingLookup, SimpleLookup};
use crate::unit::{ResUnit, ResUnits, SupUnits};

#[cfg(feature = "smallvec")]
mod maybe_small {
    pub type Vec<T> = smallvec::SmallVec<[T; 16]>;
    pub type IntoIter<T> = smallvec::IntoIter<[T; 16]>;
}
#[cfg(not(feature = "smallvec"))]
mod maybe_small {
    pub type Vec<T> = alloc::vec::Vec<T>;
    pub type IntoIter<T> = alloc::vec::IntoIter<T>;
}

#[cfg(all(feature = "std", feature = "object", feature = "memmap2"))]
/// A simple builtin split DWARF loader.
pub mod builtin_split_dwarf_loader;

mod frame;
pub use frame::{demangle, demangle_auto, Frame, FrameIter, FunctionName, Location};

mod function;
mod lazy;
mod line;

mod lookup;
pub use lookup::{LookupContinuation, LookupResult, SplitDwarfLoad};

mod unit;
pub use unit::LocationRangeIter;

type Error = gimli::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DebugFile {
    Primary,
    Supplementary,
    Dwo,
}

/// The state necessary to perform address to line translation.
///
/// Constructing a `Context` is somewhat costly, so users should aim to reuse `Context`s
/// when performing lookups for many addresses in the same executable.
pub struct Context<R: gimli::Reader> {
    sections: Arc<gimli::Dwarf<R>>,
    units: ResUnits<R>,
    sup_units: SupUnits<R>,
}

/// The type of `Context` that supports the `new` method.
#[cfg(feature = "std-object")]
pub type ObjectContext = Context<gimli::EndianRcSlice<gimli::RunTimeEndian>>;

#[cfg(feature = "std-object")]
impl Context<gimli::EndianRcSlice<gimli::RunTimeEndian>> {
    /// Construct a new `Context`.
    ///
    /// The resulting `Context` uses `gimli::EndianRcSlice<gimli::RunTimeEndian>`.
    /// This means it is not thread safe, has no lifetime constraints (since it copies
    /// the input data), and works for any endianity.
    ///
    /// Performance sensitive applications may want to use `Context::from_dwarf`
    /// with a more specialised `gimli::Reader` implementation.
    #[inline]
    pub fn new<'data, O: object::Object<'data>>(file: &O) -> Result<Self, Error> {
        Self::new_with_sup(file, None)
    }

    /// Construct a new `Context`.
    ///
    /// Optionally also use a supplementary object file.
    ///
    /// The resulting `Context` uses `gimli::EndianRcSlice<gimli::RunTimeEndian>`.
    /// This means it is not thread safe, has no lifetime constraints (since it copies
    /// the input data), and works for any endianity.
    ///
    /// Performance sensitive applications may want to use `Context::from_dwarf`
    /// with a more specialised `gimli::Reader` implementation.
    pub fn new_with_sup<'data, O: object::Object<'data>>(
        file: &O,
        sup_file: Option<&O>,
    ) -> Result<Self, Error> {
        let endian = if file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        fn load_section<'data, O, Endian>(
            id: gimli::SectionId,
            file: &O,
            endian: Endian,
        ) -> Result<gimli::EndianRcSlice<Endian>, Error>
        where
            O: object::Object<'data>,
            Endian: gimli::Endianity,
        {
            use object::ObjectSection;

            let data = file
                .section_by_name(id.name())
                .and_then(|section| section.uncompressed_data().ok())
                .unwrap_or(Cow::Borrowed(&[]));
            Ok(gimli::EndianRcSlice::new(Rc::from(&*data), endian))
        }

        let mut dwarf = gimli::Dwarf::load(|id| load_section(id, file, endian))?;
        if let Some(sup_file) = sup_file {
            dwarf.load_sup(|id| load_section(id, sup_file, endian))?;
        }
        Context::from_dwarf(dwarf)
    }
}

impl<R: gimli::Reader> Context<R> {
    /// Construct a new `Context` from DWARF sections.
    ///
    /// This method does not support using a supplementary object file.
    #[allow(clippy::too_many_arguments)]
    pub fn from_sections(
        debug_abbrev: gimli::DebugAbbrev<R>,
        debug_addr: gimli::DebugAddr<R>,
        debug_aranges: gimli::DebugAranges<R>,
        debug_info: gimli::DebugInfo<R>,
        debug_line: gimli::DebugLine<R>,
        debug_line_str: gimli::DebugLineStr<R>,
        debug_ranges: gimli::DebugRanges<R>,
        debug_rnglists: gimli::DebugRngLists<R>,
        debug_str: gimli::DebugStr<R>,
        debug_str_offsets: gimli::DebugStrOffsets<R>,
        default_section: R,
    ) -> Result<Self, Error> {
        Self::from_dwarf(gimli::Dwarf {
            debug_abbrev,
            debug_addr,
            debug_aranges,
            debug_info,
            debug_line,
            debug_line_str,
            debug_str,
            debug_str_offsets,
            debug_types: default_section.clone().into(),
            locations: gimli::LocationLists::new(
                default_section.clone().into(),
                default_section.into(),
            ),
            ranges: gimli::RangeLists::new(debug_ranges, debug_rnglists),
            file_type: gimli::DwarfFileType::Main,
            sup: None,
            abbreviations_cache: gimli::AbbreviationsCache::new(),
        })
    }

    /// Construct a new `Context` from an existing [`gimli::Dwarf`] object.
    #[inline]
    pub fn from_dwarf(sections: gimli::Dwarf<R>) -> Result<Context<R>, Error> {
        let sections = Arc::new(sections);
        let units = ResUnits::parse(&sections)?;
        let sup_units = if let Some(sup) = sections.sup.as_ref() {
            SupUnits::parse(sup)?
        } else {
            SupUnits::default()
        };
        Ok(Context {
            sections,
            units,
            sup_units,
        })
    }
}

impl<R: gimli::Reader> Context<R> {
    /// Find the DWARF unit corresponding to the given virtual memory address.
    pub fn find_dwarf_and_unit(
        &self,
        probe: u64,
    ) -> LookupResult<
        impl LookupContinuation<Output = Option<(&gimli::Dwarf<R>, &gimli::Unit<R>)>, Buf = R>,
    > {
        let mut units_iter = self.units.find(probe);
        if let Some(unit) = units_iter.next() {
            return LoopingLookup::new_lookup(
                unit.find_function_or_location(probe, self),
                move |r| {
                    ControlFlow::Break(match r {
                        Ok((Some(_), _)) | Ok((_, Some(_))) => {
                            let (_file, sections, unit) = unit
                                .dwarf_and_unit(self)
                                // We've already been through both error cases here to get to this point.
                                .unwrap()
                                .unwrap();
                            Some((sections, unit))
                        }
                        _ => match units_iter.next() {
                            Some(next_unit) => {
                                return ControlFlow::Continue(
                                    next_unit.find_function_or_location(probe, self),
                                );
                            }
                            None => None,
                        },
                    })
                },
            );
        }

        LoopingLookup::new_complete(None)
    }

    /// Find the source file and line corresponding to the given virtual memory address.
    pub fn find_location(&self, probe: u64) -> Result<Option<Location<'_>>, Error> {
        for unit in self.units.find(probe) {
            if let Some(location) = unit.find_location(probe, &self.sections)? {
                return Ok(Some(location));
            }
        }
        Ok(None)
    }

    /// Return source file and lines for a range of addresses. For each location it also
    /// returns the address and size of the range of the underlying instructions.
    pub fn find_location_range(
        &self,
        probe_low: u64,
        probe_high: u64,
    ) -> Result<LocationRangeIter<'_, R>, Error> {
        self.units
            .find_location_range(probe_low, probe_high, &self.sections)
    }

    /// Return an iterator for the function frames corresponding to the given virtual
    /// memory address.
    ///
    /// If the probe address is not for an inline function then only one frame is
    /// returned.
    ///
    /// If the probe address is for an inline function then the first frame corresponds
    /// to the innermost inline function.  Subsequent frames contain the caller and call
    /// location, until an non-inline caller is reached.
    pub fn find_frames(
        &self,
        probe: u64,
    ) -> LookupResult<impl LookupContinuation<Output = Result<FrameIter<'_, R>, Error>, Buf = R>>
    {
        let mut units_iter = self.units.find(probe);
        if let Some(unit) = units_iter.next() {
            LoopingLookup::new_lookup(unit.find_function_or_location(probe, self), move |r| {
                ControlFlow::Break(match r {
                    Err(e) => Err(e),
                    Ok((Some(function), location)) => {
                        let inlined_functions = function.find_inlined_functions(probe);
                        Ok(FrameIter::new_frames(
                            unit,
                            &self.sections,
                            function,
                            inlined_functions,
                            location,
                        ))
                    }
                    Ok((None, Some(location))) => Ok(FrameIter::new_location(location)),
                    Ok((None, None)) => match units_iter.next() {
                        Some(next_unit) => {
                            return ControlFlow::Continue(
                                next_unit.find_function_or_location(probe, self),
                            );
                        }
                        None => Ok(FrameIter::new_empty()),
                    },
                })
            })
        } else {
            LoopingLookup::new_complete(Ok(FrameIter::new_empty()))
        }
    }

    /// Preload units for `probe`.
    ///
    /// The iterator returns pairs of `SplitDwarfLoad`s containing the
    /// information needed to locate and load split DWARF for `probe` and
    /// a matching callback to invoke once that data is available.
    ///
    /// If this method is called, and all of the returned closures are invoked,
    /// addr2line guarantees that any future API call for the address `probe`
    /// will not require the loading of any split DWARF.
    ///
    /// ```no_run
    ///   # use addr2line::*;
    ///   # use std::sync::Arc;
    ///   # let ctx: Context<gimli::EndianRcSlice<gimli::RunTimeEndian>> = todo!();
    ///   # let do_split_dwarf_load = |load: SplitDwarfLoad<gimli::EndianRcSlice<gimli::RunTimeEndian>>| -> Option<Arc<gimli::Dwarf<gimli::EndianRcSlice<gimli::RunTimeEndian>>>> { None };
    ///   const ADDRESS: u64 = 0xdeadbeef;
    ///   ctx.preload_units(ADDRESS).for_each(|(load, callback)| {
    ///     let dwo = do_split_dwarf_load(load);
    ///     callback(dwo);
    ///   });
    ///
    ///   let frames_iter = match ctx.find_frames(ADDRESS) {
    ///     LookupResult::Output(result) => result,
    ///     LookupResult::Load { .. } => unreachable!("addr2line promised we wouldn't get here"),
    ///   };
    ///
    ///   // ...
    /// ```
    pub fn preload_units(
        &'_ self,
        probe: u64,
    ) -> impl Iterator<
        Item = (
            SplitDwarfLoad<R>,
            impl FnOnce(Option<Arc<gimli::Dwarf<R>>>) -> Result<(), gimli::Error> + '_,
        ),
    > {
        self.units
            .find(probe)
            .filter_map(move |unit| match unit.dwarf_and_unit(self) {
                LookupResult::Output(_) => None,
                LookupResult::Load { load, continuation } => Some((load, |result| {
                    continuation.resume(result).unwrap().map(|_| ())
                })),
            })
    }

    /// Initialize all line data structures. This is used for benchmarks.
    #[doc(hidden)]
    pub fn parse_lines(&self) -> Result<(), Error> {
        for unit in self.units.iter() {
            unit.parse_lines(&self.sections)?;
        }
        Ok(())
    }

    /// Initialize all function data structures. This is used for benchmarks.
    #[doc(hidden)]
    pub fn parse_functions(&self) -> Result<(), Error> {
        for unit in self.units.iter() {
            unit.parse_functions(self).skip_all_loads()?;
        }
        Ok(())
    }

    /// Initialize all inlined function data structures. This is used for benchmarks.
    #[doc(hidden)]
    pub fn parse_inlined_functions(&self) -> Result<(), Error> {
        for unit in self.units.iter() {
            unit.parse_inlined_functions(self).skip_all_loads()?;
        }
        Ok(())
    }
}

impl<R: gimli::Reader> Context<R> {
    // Find the unit containing the given offset, and convert the offset into a unit offset.
    fn find_unit(
        &self,
        offset: gimli::DebugInfoOffset<R::Offset>,
        file: DebugFile,
    ) -> Result<(&gimli::Unit<R>, gimli::UnitOffset<R::Offset>), Error> {
        let unit = match file {
            DebugFile::Primary => self.units.find_offset(offset)?,
            DebugFile::Supplementary => self.sup_units.find_offset(offset)?,
            DebugFile::Dwo => return Err(gimli::Error::NoEntryAtGivenOffset),
        };

        let unit_offset = offset
            .to_unit_offset(&unit.header)
            .ok_or(gimli::Error::NoEntryAtGivenOffset)?;
        Ok((unit, unit_offset))
    }
}

struct RangeAttributes<R: gimli::Reader> {
    low_pc: Option<u64>,
    high_pc: Option<u64>,
    size: Option<u64>,
    ranges_offset: Option<gimli::RangeListsOffset<<R as gimli::Reader>::Offset>>,
}

impl<R: gimli::Reader> Default for RangeAttributes<R> {
    fn default() -> Self {
        RangeAttributes {
            low_pc: None,
            high_pc: None,
            size: None,
            ranges_offset: None,
        }
    }
}

impl<R: gimli::Reader> RangeAttributes<R> {
    fn for_each_range<F: FnMut(gimli::Range)>(
        &self,
        sections: &gimli::Dwarf<R>,
        unit: &gimli::Unit<R>,
        mut f: F,
    ) -> Result<bool, Error> {
        let mut added_any = false;
        let mut add_range = |range: gimli::Range| {
            if range.begin < range.end {
                f(range);
                added_any = true
            }
        };
        if let Some(ranges_offset) = self.ranges_offset {
            let mut range_list = sections.ranges(unit, ranges_offset)?;
            while let Some(range) = range_list.next()? {
                add_range(range);
            }
        } else if let (Some(begin), Some(end)) = (self.low_pc, self.high_pc) {
            add_range(gimli::Range { begin, end });
        } else if let (Some(begin), Some(size)) = (self.low_pc, self.size) {
            add_range(gimli::Range {
                begin,
                end: begin + size,
            });
        }
        Ok(added_any)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn context_is_send() {
        fn assert_is_send<T: Send>() {}
        assert_is_send::<crate::Context<gimli::read::EndianSlice<'_, gimli::LittleEndian>>>();
    }
}
