use itertools::izip;
use plonky2::field::extension::Extendable;
use plonky2::field::packed::PackedField;
use plonky2::field::types::Field;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::ext_target::ExtensionTarget;

use super::columns::COL_MAP;
use crate::constraint_consumer::{ConstraintConsumer, RecursiveConstraintConsumer};
use crate::cpu::columns::ops::OpsColumnsView;
use crate::cpu::columns::CpuColumnsView;

const KERNEL_ONLY_INSTR: Option<u32> = Some(0);
const G_JUMPDEST: Option<u32> = Some(1);
const G_BASE: Option<u32> = Some(2);
const G_VERYLOW: Option<u32> = Some(3);
const G_LOW: Option<u32> = Some(5);
const G_MID: Option<u32> = Some(8);
const G_HIGH: Option<u32> = Some(10);

const SIMPLE_OPCODES: OpsColumnsView<Option<u32>> = OpsColumnsView {
    add: G_VERYLOW,
    mul: G_LOW,
    sub: G_VERYLOW,
    div: G_LOW,
    mod_: G_LOW,
    addmod: G_MID,
    mulmod: G_MID,
    addfp254: KERNEL_ONLY_INSTR,
    mulfp254: KERNEL_ONLY_INSTR,
    subfp254: KERNEL_ONLY_INSTR,
    submod: KERNEL_ONLY_INSTR,
    lt: G_VERYLOW,
    gt: G_VERYLOW,
    eq_iszero: G_VERYLOW,
    logic_op: G_VERYLOW,
    not: G_VERYLOW,
    byte: G_VERYLOW,
    shl: G_VERYLOW,
    shr: G_VERYLOW,
    keccak_general: KERNEL_ONLY_INSTR,
    prover_input: KERNEL_ONLY_INSTR,
    pop: G_BASE,
    jumps: None, // Combined flag handled separately.
    pc: G_BASE,
    jumpdest: G_JUMPDEST,
    push0: G_BASE,
    push: G_VERYLOW,
    dup: G_VERYLOW,
    swap: G_VERYLOW,
    context_op: KERNEL_ONLY_INSTR,
    exit_kernel: None,
    mload_general: KERNEL_ONLY_INSTR,
    mstore_general: KERNEL_ONLY_INSTR,
    syscall: None,
    exception: None,
};

fn eval_packed_accumulate<P: PackedField>(
    lv: &CpuColumnsView<P>,
    nv: &CpuColumnsView<P>,
    yield_constr: &mut ConstraintConsumer<P>,
) {
    // Is it an instruction that we constrain here?
    // I.e., does it always cost a constant amount of gas?
    let filter: P = SIMPLE_OPCODES
        .into_iter()
        .enumerate()
        .filter_map(|(i, maybe_cost)| {
            // Add flag `lv.op[i]` to the sum if `SIMPLE_OPCODES[i]` is `Some`.
            maybe_cost.map(|_| lv.op[i])
        })
        .sum();

    // How much gas did we use?
    let gas_used: P = SIMPLE_OPCODES
        .into_iter()
        .enumerate()
        .filter_map(|(i, maybe_cost)| {
            maybe_cost.map(|cost| P::Scalar::from_canonical_u32(cost) * lv.op[i])
        })
        .sum();

    let constr = nv.gas - (lv.gas + gas_used);
    yield_constr.constraint_transition(filter * constr);

    for (maybe_cost, op_flag) in izip!(SIMPLE_OPCODES.into_iter(), lv.op.into_iter()) {
        if let Some(cost) = maybe_cost {
            let cost = P::Scalar::from_canonical_u32(cost);
            yield_constr.constraint_transition(op_flag * (nv.gas - lv.gas - cost));
        }
    }

    // For jumps.
    let jump_gas_cost = P::Scalar::from_canonical_u32(G_MID.unwrap())
        + lv.opcode_bits[0] * P::Scalar::from_canonical_u32(G_HIGH.unwrap() - G_MID.unwrap());
    yield_constr.constraint_transition(lv.op.jumps * (nv.gas - lv.gas - jump_gas_cost));
}

fn eval_packed_init<P: PackedField>(
    lv: &CpuColumnsView<P>,
    nv: &CpuColumnsView<P>,
    yield_constr: &mut ConstraintConsumer<P>,
) {
    let is_cpu_cycle: P = COL_MAP.op.iter().map(|&col_i| lv[col_i]).sum();
    let is_cpu_cycle_next: P = COL_MAP.op.iter().map(|&col_i| nv[col_i]).sum();
    // `nv` is the first row that executes an instruction.
    let filter = (is_cpu_cycle - P::ONES) * is_cpu_cycle_next;
    // Set initial gas to zero.
    yield_constr.constraint_transition(filter * nv.gas);
}

pub fn eval_packed<P: PackedField>(
    lv: &CpuColumnsView<P>,
    nv: &CpuColumnsView<P>,
    yield_constr: &mut ConstraintConsumer<P>,
) {
    eval_packed_accumulate(lv, nv, yield_constr);
    eval_packed_init(lv, nv, yield_constr);
}

fn eval_ext_circuit_accumulate<F: RichField + Extendable<D>, const D: usize>(
    builder: &mut plonky2::plonk::circuit_builder::CircuitBuilder<F, D>,
    lv: &CpuColumnsView<ExtensionTarget<D>>,
    nv: &CpuColumnsView<ExtensionTarget<D>>,
    yield_constr: &mut RecursiveConstraintConsumer<F, D>,
) {
    // Is it an instruction that we constrain here?
    // I.e., does it always cost a constant amount of gas?
    let filter = SIMPLE_OPCODES.into_iter().enumerate().fold(
        builder.zero_extension(),
        |cumul, (i, maybe_cost)| {
            // Add flag `lv.op[i]` to the sum if `SIMPLE_OPCODES[i]` is `Some`.
            match maybe_cost {
                None => cumul,
                Some(_) => builder.add_extension(lv.op[i], cumul),
            }
        },
    );

    // How much gas did we use?
    let gas_used = SIMPLE_OPCODES.into_iter().enumerate().fold(
        builder.zero_extension(),
        |cumul, (i, maybe_cost)| match maybe_cost {
            None => cumul,
            Some(cost) => {
                let cost_ext = builder.constant_extension(F::from_canonical_u32(cost).into());
                builder.mul_add_extension(lv.op[i], cost_ext, cumul)
            }
        },
    );

    let constr = {
        let t = builder.add_extension(lv.gas, gas_used);
        builder.sub_extension(nv.gas, t)
    };
    let filtered_constr = builder.mul_extension(filter, constr);
    yield_constr.constraint_transition(builder, filtered_constr);

    for (maybe_cost, op_flag) in izip!(SIMPLE_OPCODES.into_iter(), lv.op.into_iter()) {
        if let Some(cost) = maybe_cost {
            let nv_lv_diff = builder.sub_extension(nv.gas, lv.gas);
            let constr = builder.arithmetic_extension(
                F::ONE,
                -F::from_canonical_u32(cost),
                op_flag,
                nv_lv_diff,
                op_flag,
            );
            yield_constr.constraint_transition(builder, constr);
        }
    }

    // For jumps.
    let filter = lv.op.jumps;
    let jump_gas_cost = builder.mul_const_extension(
        F::from_canonical_u32(G_HIGH.unwrap() - G_MID.unwrap()),
        lv.opcode_bits[0],
    );
    let jump_gas_cost =
        builder.add_const_extension(jump_gas_cost, F::from_canonical_u32(G_MID.unwrap()));

    let nv_lv_diff = builder.sub_extension(nv.gas, lv.gas);
    let gas_diff = builder.sub_extension(nv_lv_diff, jump_gas_cost);
    let constr = builder.mul_extension(filter, gas_diff);
    yield_constr.constraint_transition(builder, constr);
}

fn eval_ext_circuit_init<F: RichField + Extendable<D>, const D: usize>(
    builder: &mut plonky2::plonk::circuit_builder::CircuitBuilder<F, D>,
    lv: &CpuColumnsView<ExtensionTarget<D>>,
    nv: &CpuColumnsView<ExtensionTarget<D>>,
    yield_constr: &mut RecursiveConstraintConsumer<F, D>,
) {
    // `nv` is the first row that executes an instruction.
    let is_cpu_cycle = builder.add_many_extension(COL_MAP.op.iter().map(|&col_i| lv[col_i]));
    let is_cpu_cycle_next = builder.add_many_extension(COL_MAP.op.iter().map(|&col_i| nv[col_i]));
    let filter = builder.mul_sub_extension(is_cpu_cycle, is_cpu_cycle_next, is_cpu_cycle_next);
    // Set initial gas to zero.
    let constr = builder.mul_extension(filter, nv.gas);
    yield_constr.constraint_transition(builder, constr);
}

pub fn eval_ext_circuit<F: RichField + Extendable<D>, const D: usize>(
    builder: &mut plonky2::plonk::circuit_builder::CircuitBuilder<F, D>,
    lv: &CpuColumnsView<ExtensionTarget<D>>,
    nv: &CpuColumnsView<ExtensionTarget<D>>,
    yield_constr: &mut RecursiveConstraintConsumer<F, D>,
) {
    eval_ext_circuit_accumulate(builder, lv, nv, yield_constr);
    eval_ext_circuit_init(builder, lv, nv, yield_constr);
}
