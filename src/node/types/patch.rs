//! Testnet patches for the reth block conversion in [`super::reth_compat`].
use super::LegacyReceipt;
use crate::chainspec::TESTNET_CHAIN_ID;
use alloy_primitives::{Address, B256, U256, address, b256};

/// `keccak256("Transfer(address,address,uint256)")` — the ERC-20 `Transfer` topic.
const ERC20_TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// Token contract whose testnet system transactions need real-sender recovery.
///
/// Its synthetic spot-index sender (`s_to_address(to_s(0))` == `0x2000…0000`)
/// collides across holders, so the interleaved per-sender nonces only validate
/// once the true `msg.sender` is recovered from the receipt.
const SENDER_RECOVERY_TOKEN: Address = address!("0x2b3370ee501b4a559b57d449569354196457d8ab");

/// First testnet block at which the recovery applies.
const SENDER_RECOVERY_FROM_BLOCK: u64 = 55231857;

/// Recover the real `msg.sender` of a token system transaction from its receipt,
/// restricted to the minimal known-affected set (testnet, one token, at or after
/// the first affected block). Returns `None` everywhere else so the caller falls
/// back to the legacy synthetic-sender (`to_s`) encoding.
///
/// For an ERC-20 transfer the caller is observable as the `from` topic of the
/// `Transfer` event emitted by the token (`to`) contract.
pub(super) fn recover_testnet_system_tx_sender(
    chain_id: u64,
    block_number: u64,
    token: Address,
    receipt: Option<&LegacyReceipt>,
) -> Option<Address> {
    if chain_id != TESTNET_CHAIN_ID
        || block_number < SENDER_RECOVERY_FROM_BLOCK
        || token != SENDER_RECOVERY_TOKEN
    {
        return None;
    }
    let sender = receipt?.logs.iter().find_map(|log| {
        let topics = log.data.topics();
        (log.address == token
            && topics.first() == Some(&ERC20_TRANSFER_TOPIC)
            && topics.len() >= 2)
            .then(|| Address::from_word(topics[1]))
    })?;
    // `s_to_address` treats `s == 1` as a sentinel (mapping it to 0x2222…2222), so a
    // sender of 0x00..01 cannot be round-tripped through `s`. Decline it (fall back
    // to the legacy encoding) rather than mis-recovering.
    (U256::from_be_slice(sender.as_slice()) != U256::ONE).then_some(sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::types::LegacyReceipt;
    use alloy_consensus::TxType;
    use alloy_primitives::{Bytes, Log, LogData};
    use reth_ethereum_primitives::EthereumReceipt;

    const HOLDER_A: Address = address!("f9b10ef826e9aa275f1813034e3bd9b80224e535");
    const HOLDER_B: Address = address!("0b80659a4076e9e93c7dbe0f10675a16a3e5c206");
    const OTHER_TOKEN: Address = address!("d9cbec81df392a88aeff575e962d149d57f4d6bc");

    fn transfer_log(emitter: Address, from: Address, to: Address) -> Log {
        Log {
            address: emitter,
            data: LogData::new_unchecked(
                vec![ERC20_TRANSFER_TOPIC, from.into_word(), to.into_word()],
                U256::from(1_000u64).to_be_bytes::<32>().to_vec().into(),
            ),
        }
    }

    fn receipt(logs: Vec<Log>) -> LegacyReceipt {
        EthereumReceipt { tx_type: TxType::Legacy, success: true, cumulative_gas_used: 0, logs }
            .into()
    }

    fn recover(chain_id: u64, block: u64, token: Address, logs: Vec<Log>) -> Option<Address> {
        let r = receipt(logs);
        recover_testnet_system_tx_sender(chain_id, block, token, Some(&r))
    }

    #[test]
    fn recovers_sender_for_targeted_token() {
        let logs = vec![transfer_log(SENDER_RECOVERY_TOKEN, HOLDER_B, HOLDER_A)];
        assert_eq!(
            recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK, SENDER_RECOVERY_TOKEN, logs),
            Some(HOLDER_B)
        );
    }

    #[test]
    fn declines_outside_the_minimal_set() {
        let log = || vec![transfer_log(SENDER_RECOVERY_TOKEN, HOLDER_B, HOLDER_A)];
        // wrong chain
        assert_eq!(recover(999, SENDER_RECOVERY_FROM_BLOCK, SENDER_RECOVERY_TOKEN, log()), None);
        // before the first affected block
        assert_eq!(
            recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK - 1, SENDER_RECOVERY_TOKEN, log()),
            None
        );
        // a different (non-targeted) token, even though it emits a Transfer
        let other = vec![transfer_log(OTHER_TOKEN, HOLDER_B, HOLDER_A)];
        assert_eq!(recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK, OTHER_TOKEN, other), None);
    }

    #[test]
    fn ignores_unrelated_logs() {
        // Transfer emitted by some other contract (not the token `to`).
        let foreign = vec![transfer_log(OTHER_TOKEN, HOLDER_B, HOLDER_A)];
        assert_eq!(
            recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK, SENDER_RECOVERY_TOKEN, foreign),
            None
        );
        // Non-Transfer event (wrong topic0) from the token.
        let approval = Log {
            address: SENDER_RECOVERY_TOKEN,
            data: LogData::new_unchecked(
                vec![B256::ZERO, HOLDER_B.into_word(), HOLDER_A.into_word()],
                Bytes::new(),
            ),
        };
        assert_eq!(
            recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK, SENDER_RECOVERY_TOKEN, vec![
                approval
            ]),
            None
        );
    }

    #[test]
    fn guards_against_degenerate_one_address() {
        let one = address!("0000000000000000000000000000000000000001");
        let logs = vec![transfer_log(SENDER_RECOVERY_TOKEN, one, HOLDER_A)];
        assert_eq!(
            recover(TESTNET_CHAIN_ID, SENDER_RECOVERY_FROM_BLOCK, SENDER_RECOVERY_TOKEN, logs),
            None
        );
    }
}
