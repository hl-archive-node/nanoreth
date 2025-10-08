//! Modified version of `blockhash` instruction before block `243538`.
//!
//! This is a mainnet-specific fix for the `blockhash` instruction,
//! copied and modified from revm-interpreter-25.0.1/src/instructions/host.rs.

use alloy_primitives::keccak256;
use revm::{
    context::Host,
    interpreter::{
        as_u64_saturated, interpreter_types::StackTr, popn_top, InstructionContext,
        InterpreterTypes,
    },
    primitives::{BLOCK_HASH_HISTORY, U256},
};

#[doc(hidden)]
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! _count {
    (@count) => { 0 };
    (@count $head:tt $($tail:tt)*) => { 1 + _count!(@count $($tail)*) };
    ($($arg:tt)*) => { _count!(@count $($arg)*) };
}

/// Pops n values from the stack and returns the top value. Fails the instruction if n values can't
/// be popped.
#[macro_export]
#[collapse_debuginfo(yes)]
macro_rules! popn_top {
    ([ $($x:ident),* ], $top:ident, $interpreter:expr $(,$ret:expr)? ) => {
        // Workaround for https://github.com/rust-lang/rust/issues/144329.
        if $interpreter.stack.len() < (1 + $crate::_count!($($x)*)) {
            $interpreter.halt_underflow();
            return $($ret)?;
        }
        let ([$( $x ),*], $top) = unsafe { $interpreter.stack.popn_top().unwrap_unchecked() };
    };
}

/// Implements the BLOCKHASH instruction.
///
/// Gets the hash of one of the 256 most recent complete blocks.
pub fn blockhash_returning_placeholder<WIRE: InterpreterTypes, H: Host + ?Sized>(
    context: InstructionContext<'_, H, WIRE>,
) {
    //gas!(context.interpreter, gas::BLOCKHASH);
    popn_top!([], number, context.interpreter);

    let requested_number = *number;
    let block_number = context.host.block_number();

    let Some(diff) = block_number.checked_sub(requested_number) else {
        *number = U256::ZERO;
        return;
    };

    let diff = as_u64_saturated!(diff);

    // blockhash should push zero if number is same as current block number.
    if diff == 0 {
        *number = U256::ZERO;
        return;
    }

    *number = if diff <= BLOCK_HASH_HISTORY {
        // NOTE: This is HL-specific modifcation that returns the placeholder hash before specific
        // block.
        let hash = keccak256(as_u64_saturated!(requested_number).to_string().as_bytes());
        U256::from_be_bytes(hash.0)
    } else {
        U256::ZERO
    }
}
