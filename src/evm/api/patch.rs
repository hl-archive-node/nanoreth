//! Modified version of `blockhash` instruction before block `243538`.
//!
//! This is a mainnet-specific fix for the `blockhash` instruction,
//! copied and modified from revm-interpreter-25.0.1/src/instructions/host.rs.

use alloy_primitives::keccak256;
use revm::{
    context::Host,
    interpreter::{
        InstructionContext, InstructionExecResult, InterpreterTypes, as_u64_saturated,
        interpreter_types::StackTr, popn_top,
    },
    primitives::{BLOCK_HASH_HISTORY, U256},
};

/// Implements the BLOCKHASH instruction.
///
/// Gets the hash of one of the 256 most recent complete blocks.
pub fn blockhash_returning_placeholder<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) -> InstructionExecResult {
    //gas!(context.interpreter, gas::BLOCKHASH);
    popn_top!([], number, context.interpreter);

    let requested_number = *number;
    let block_number = context.host.block_number();

    let Some(diff) = block_number.checked_sub(requested_number) else {
        *number = U256::ZERO;
        return Ok(());
    };

    let diff = as_u64_saturated!(diff);

    // blockhash should push zero if number is same as current block number.
    if diff == 0 {
        *number = U256::ZERO;
        return Ok(());
    }

    *number = if diff <= BLOCK_HASH_HISTORY {
        // NOTE: This is HL-specific modifcation that returns the placeholder hash before specific
        // block.
        let hash = keccak256(as_u64_saturated!(requested_number).to_string().as_bytes());
        U256::from_be_bytes(hash.0)
    } else {
        U256::ZERO
    };
    Ok(())
}
