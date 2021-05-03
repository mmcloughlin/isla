// BSD 2-Clause License
//
// Copyright (c) 2020 Alasdair Armstrong
//
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
// 1. Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright
// notice, this list of conditions and the following disclaimer in the
// documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
// HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
// SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
// LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
// DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
// THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
// (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use crossbeam::queue::SegQueue;
use getopts::Matches;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::convert::TryInto;
use std::process::exit;
use std::sync::Arc;
use std::time::Instant;
use std::path::PathBuf;

use isla_axiomatic::footprint_analysis::footprint_analysis;
use isla_axiomatic::litmus::assemble_instruction;
use isla_axiomatic::page_table::{PageTables, S1PageAttrs, S2PageAttrs};
use isla_lib::bitvector::{b129::B129, BV};
use isla_lib::executor;
use isla_lib::executor::{LocalFrame, TaskState};
use isla_lib::init::{initialize_architecture, Initialized};
use isla_lib::ir::source_loc::SourceLoc;
use isla_lib::ir::*;
use isla_lib::memory::{Memory, Region};
use isla_lib::smt;
use isla_lib::smt::{smtlib, EvPath, Event, Solver};
use isla_lib::smt_parser;
use isla_lib::zencode;
use isla_lib::{simplify, simplify::WriteOpts};

mod opts;
use opts::CommonOpts;

fn main() {
    let code = isla_main();
    unsafe { isla_lib::smt::finalize_solver() };
    exit(code)
}

pub fn hex_bytes(s: &str) -> Result<Vec<u8>, std::num::ParseIntError> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16)).collect()
}

#[derive(Clone, Debug)]
enum InstructionSegment {
    Concrete(B129),
    Symbolic(String, u32),
}

impl std::fmt::Display for InstructionSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            InstructionSegment::Concrete(bv) => bv.fmt(f),
            InstructionSegment::Symbolic(s, _) => s.fmt(f),
        }
    }
}

fn instruction_to_string(opcode: &[InstructionSegment]) -> String {
    let mut s = "".to_string();
    for seg in opcode {
        s += &format!("{} ", seg);
    }
    s
}

fn instruction_to_val(opcode: &[InstructionSegment], matches: &Matches, solver: &mut Solver<B129>) -> Val<B129> {
    match opcode {
        [InstructionSegment::Concrete(bv)] => Val::Bits(*bv),
        _ => {
            let mut var_map = HashMap::new();
            let val = Val::MixedBits(
                opcode
                    .iter()
                    .map(|segment| match segment {
                        InstructionSegment::Concrete(bv) => BitsSegment::Concrete(*bv),
                        InstructionSegment::Symbolic(name, size) => {
                            if let Some((size2, v)) = var_map.get(name) {
                                if size == size2 {
                                    BitsSegment::Symbolic(*v)
                                } else {
                                    panic!(
                                        "{} appears in instruction with different sizes, {} and {}",
                                        name, size, size2
                                    )
                                }
                            } else {
                                let v = solver.declare_const(smtlib::Ty::BitVec(*size), SourceLoc::unknown());
                                var_map.insert(name, (*size, v));
                                BitsSegment::Symbolic(v)
                            }
                        }
                    })
                    .collect(),
            );
            for constraint in matches.opt_strs("instruction-constraint") {
                let mut lookup = |loc: Loc<String>| match loc {
                    Loc::Id(name) => match var_map.get(&zencode::decode(&name)) {
                        Some((_size, v)) => Ok(smtlib::Exp::Var(*v)),
                        None => Err(format!("No variable {} in constraint", name)),
                    },
                    _ => Err(format!("Only names can appear in instruction constraints, not {}", loc)),
                };
                let assertion =
                    smt_parser::ExpParser::new().parse(&mut lookup, &constraint).expect("Bad instruction constraint");
                solver.add(smtlib::Def::Assert(assertion));
            }
            val
        }
    }
}

fn opcode_bytes(opcode: Vec<u8>, little_endian: bool) -> B129 {
    if opcode.len() > 8 {
        eprintln!("Currently instructions greater than 8 bytes in length are not supported");
        exit(1);
    }

    if opcode.len() == 2 {
        let opcode: Box<[u8; 2]> = opcode.into_boxed_slice().try_into().unwrap();
        B129::from_u16(if little_endian { u16::from_le_bytes(*opcode) } else { u16::from_be_bytes(*opcode) })
    } else if opcode.len() == 4 {
        let opcode: Box<[u8; 4]> = opcode.into_boxed_slice().try_into().unwrap();
        B129::from_u32(if little_endian { u32::from_le_bytes(*opcode) } else { u32::from_be_bytes(*opcode) })
    } else {
        B129::from_bytes(&opcode)
    }
}

fn isla_main() -> i32 {
    let mut opts = opts::common_opts();
    opts.reqopt("i", "instruction", "display footprint of instruction", "<instruction>");
    opts.optopt("e", "endianness", "instruction encoding endianness (default: little)", "big/little");
    opts.optflag("d", "dependency", "view instruction dependency info");
    opts.optflag("x", "hex", "parse instruction as hexadecimal opcode, rather than assembly");
    opts.optflag("s", "simplify", "simplify instruction footprint");
    opts.optopt("f", "function", "use a custom footprint function", "<identifer>");
    opts.optflag("c", "continue-on-error", "continue generating traces upon encountering an error");
    opts.optopt("", "source", "Sail source code directory for .ir file", "<path>");
    opts.optmulti("", "identity-map", "set up an identity mapping for the provided address", "<address>");
    opts.optflag("", "create-memory-regions", "create default memory regions");
    opts.optflag("", "partial", "parse instruction as binary with unknown bits");
    opts.optmulti("", "instruction-constraint", "add constraint on variables in a partial instruction", "<constraint>");

    let mut hasher = Sha256::new();
    let (matches, arch) = opts::parse(&mut hasher, &opts);
    let CommonOpts { num_threads, mut arch, symtab, isa_config } =
        opts::parse_with_arch(&mut hasher, &opts, &matches, &arch);

    let Initialized { regs, lets, shared_state } =
        initialize_architecture(&mut arch, symtab, &isa_config, AssertionMode::Optimistic);

    let little_endian = match matches.opt_str("endianness").as_deref() {
        Some("little") | None => true,
        Some("big") => false,
        Some(_) => {
            eprintln!("--endianness argument must be one of either `big` or `little`");
            exit(1)
        }
    };

    let instruction = matches.opt_str("instruction").unwrap();

    let opcode: Vec<InstructionSegment> = if matches.opt_present("partial") {
        instruction.split_ascii_whitespace().map(
            |s| B129::from_str(&format!("0b{}", s))
                .map(|bv| InstructionSegment::Concrete(bv))
                .or_else(
                    || {
                        let mut it = s.split(':');
                        let name = it.next()?;
                        let size = it.next()?;
                        u32::from_str_radix(size, 10)
                            .ok()
                            .map(|size| InstructionSegment::Symbolic(name.to_string(), size))
                    })
                .unwrap_or_else(
                    || { eprintln!("Unable to parse instruction segment {}", s);
                         exit(1)
                    })
        ).collect()
    } else if matches.opt_present("hex") {
        match hex_bytes(&instruction) {
            Ok(opcode) => vec![InstructionSegment::Concrete(opcode_bytes(opcode, little_endian))],
            Err(e) => {
                eprintln!("Could not parse hexadecimal opcode: {}", e);
                exit(1)
            }
        }
    } else {
        match assemble_instruction(&instruction, &isa_config) {
            Ok(opcode) => vec![InstructionSegment::Concrete(opcode_bytes(opcode, little_endian))],
            Err(msg) => {
                eprintln!("{}", msg);
                return 1;
            }
        }
    };

    eprintln!("opcode: {}", instruction_to_string(&opcode));

    let mut memory = Memory::new();

    let mut s1_tables = PageTables::new("stage 1", isa_config.page_table_base);
    let mut s2_tables = PageTables::new("stage 2", isa_config.s2_page_table_base);
    let s1_level0 = s1_tables.alloc();
    let s2_level0 = s2_tables.alloc();

    matches.opt_strs("identity-map").iter().for_each(|addr| {
        if let Some(addr) = B129::from_str(addr) {
            s1_tables.identity_map(s1_level0, addr.lower_u64(), S1PageAttrs::default());
            s2_tables.identity_map(s2_level0, addr.lower_u64(), S2PageAttrs::default());
        } else {
            eprintln!("Could not parse address {} in --identity-map argument", addr);
            exit(1)
        }
    });

    let mut page = isa_config.page_table_base;
    while page < s1_tables.range().end {
        s2_tables.identity_map(s2_level0, page, S2PageAttrs::default());
        page += isa_config.page_size
    }

    if matches.opt_present("create-memory-regions") {
        memory.add_region(Region::Custom(s1_tables.range(), Box::new(s1_tables.freeze())));
        memory.add_region(Region::Custom(s2_tables.range(), Box::new(s2_tables.freeze())));

        memory.add_zero_region(0x0..0xffff_ffff_ffff_ffff);
    }

    let footprint_function = match matches.opt_str("function") {
        Some(id) => zencode::encode(&id),
        None => "zisla_footprint".to_string(),
    };

    let (initial_checkpoint, opcode_val) = {
        let solver_cfg = smt::Config::new();
        let solver_ctx = smt::Context::new(solver_cfg);
        let mut solver = Solver::new(&solver_ctx);
        let opcode_val = instruction_to_val(&opcode, &matches, &mut solver);
        (smt::checkpoint(&mut solver), opcode_val)
    };

    let function_id = shared_state.symtab.lookup(&footprint_function);
    let (args, _, instrs) = shared_state.functions.get(&function_id).unwrap();
    let task_state = TaskState::new();
    let task = LocalFrame::new(function_id, args, Some(&[opcode_val.clone()]), instrs)
        .add_lets(&lets)
        .add_regs(&regs)
        .set_memory(memory)
        .task_with_checkpoint(0, &task_state, initial_checkpoint);

    let queue = Arc::new(SegQueue::new());

    let now = Instant::now();
    executor::start_multi(num_threads, None, vec![task], &shared_state, queue.clone(), &executor::trace_collector);
    eprintln!("Execution took: {}ms", now.elapsed().as_millis());

    let mut paths = Vec::new();
    let rk_ifetch = shared_state.enum_member(isa_config.ifetch_read_kind).expect("Invalid ifetch read kind");

    loop {
        match queue.pop() {
            Ok(Ok((_, mut events))) if matches.opt_present("dependency") => {
                let mut events: EvPath<B129> = events
                    .drain(..)
                    .rev()
                    .filter(|ev| {
                        (ev.is_memory() && !ev.has_read_kind(rk_ifetch))
                            || ev.is_smt()
                            || ev.is_instr()
                            || ev.is_cycle()
                            || ev.is_write_reg()
                    })
                    .collect();
                simplify::remove_unused(&mut events);
                events.push(Event::Instr(opcode_val.clone()));
                paths.push(events)
            }
            Ok(Ok((_, mut events))) => {
                if matches.opt_present("simplify") {
                    simplify::hide_initialization(&mut events);
                    simplify::remove_unused(&mut events);
                    simplify::propagate_forwards_used_once(&mut events);
                    simplify::commute_extract(&mut events);
                    simplify::eval(&mut events);
                }
                let events: Vec<Event<B129>> = events.drain(..).rev().collect();
                let stdout = std::io::stdout();
                let mut handle = stdout.lock();
                let write_opts = WriteOpts { define_enum: !matches.opt_present("simplify"), source_directory: matches.opt_str("source").map(PathBuf::from), ..WriteOpts::default() };
                simplify::write_events_with_opts(&mut handle, &events, &shared_state.symtab, &write_opts).unwrap();
            }
            // Error during execution
            Ok(Err(msg)) => {
                eprintln!("{}", msg);
                if !matches.opt_present("continue-on-error") {
                    return 1;
                }
            }
            // Empty queue
            Err(_) => break,
        }
    }

    if matches.opt_present("dependency") {
        match footprint_analysis(num_threads, &[paths], &lets, &regs, &shared_state, &isa_config, None) {
            Ok(footprints) => {
                for (_, footprint) in footprints {
                    {
                        let stdout = std::io::stdout();
                        let mut handle = stdout.lock();
                        let _ = footprint.pretty(&mut handle, &shared_state.symtab);
                    }
                }
            }
            Err(footprint_error) => {
                eprintln!("{:?}", footprint_error);
                return 1;
            }
        }
    }

    0
}
