// BSD 2-Clause License
//
// Copyright (c) 2019, 2020 Alasdair Armstrong
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

use crate::concrete::BV;
use crate::ir::*;
use crate::log;
use crate::simplify::EventReferences;
use crate::smt::Solver;
use crate::zencode;

pub fn args_info<B: BV>(tid: usize, args: &[Val<B>], shared_state: &SharedState<B>, solver: &Solver<B>) {
    let events = solver.trace().to_vec();
    let references = EventReferences::from_events(&events);

    for arg in args {
        if let Val::Symbolic(sym) = arg {
            let (taints, memory) = references.taints(*sym, &events);
            let taints: Vec<String> =
                taints.iter().map(|(reg, _)| zencode::decode(shared_state.symtab.to_str(*reg))).collect();
            let memory = if memory { ", MEMORY" } else { "" };
            log_from!(tid, log::PROBE, &format!("Symbol {} taints: {:?}{}", sym, taints, memory))
        }
    }
}
