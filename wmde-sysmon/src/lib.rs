// SPDX-License-Identifier: GPL-3.0-only

//! Load sampling and the graph widget for the WMDE stack.
//!
//! [`probe`] turns `/proc` counters into one [`probe::Sample`] per tick and is plain `std`.
//! [`graph`] draws the samples and knows only `cosmic::`. Neither reads a config nor a
//! locale: the consumer owns both.

pub mod graph;
pub mod probe;
