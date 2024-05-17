//! A tx for a validator to change their consensus key.

use booleans::ResultBoolExt;
use namada_tx_prelude::transaction::pos::ConsensusKeyChange;
use namada_tx_prelude::*;

#[transaction]
fn apply_tx(ctx: &mut Ctx, tx_data: Tx) -> TxResult {
    let signed = tx_data;
    let data = signed.data().ok_or_err_msg("Missing data")?;
    let ConsensusKeyChange {
        validator,
        consensus_key,
    } = transaction::pos::ConsensusKeyChange::try_from_slice(&data[..])
        .wrap_err("Failed to decode ConsensusKeyChange value")?;

    // Check that the tx has been signed with the new consensus key
    verify_signatures_of_pks(ctx, &signed, vec![consensus_key.clone()])
        .true_or_else(|| {
            const ERR_MSG: &str =
                "Consensus key ownership signature verification failed";
            debug_log!("{ERR_MSG}");
            Error::new_const(ERR_MSG)
        })?;

    ctx.change_validator_consensus_key(&validator, &consensus_key)
        .wrap_err("Failed to change validator consensus key")
}
