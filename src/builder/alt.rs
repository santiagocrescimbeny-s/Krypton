//! Address Lookup Table (ALT) support for the atomic launch.
//!
//! A launch touches about 25 accounts; as plain 32-byte keys they take a
//! `v0` transaction to roughly 1212 bytes (limit: 1232). Eleven of those
//! accounts are stable program-wide addresses (see
//! [`super::pda::lookup_table_addresses`]); once they live in a table each one
//! costs a single index byte, which brings the transaction down to about 905
//! bytes and leaves room for the Jito tip (Phase 4).
//!
//! This module provides:
//! - hand-encoded `CreateLookupTable` / `ExtendLookupTable` instructions
//!   (`solana-sdk` 4.1 does not ship the ALT interface), plus the table-address
//!   derivation;
//! - a decoder for the on-chain table account;
//! - [`LaunchLookupTable`], the checked bridge between a table and
//!   [`LaunchBuilder::compile_with_table`](super::transaction::LaunchBuilder::compile_with_table).
//!
//! # Rules the network imposes
//! - Addresses added to a table are usable only from the **next slot**
//!   ([`DecodedLookupTable::is_usable_at`]); a transaction that references them
//!   in the same slot fails.
//! - **Never put the Jito tip accounts in a table.** Jito's documentation says
//!   so explicitly: the tip must reach the account as a plain static key. The
//!   launch table therefore holds no tip account, and the tip instruction keeps
//!   its accounts static.
//! - Programs invoked directly by the transaction (Pump.fun, ATA, Compute
//!   Budget and, with a tip, the System program) are never loaded from a table.

use solana_pubkey::Pubkey;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::AddressLookupTableAccount;

use super::constants::SYSTEM_PROGRAM_ID;
use super::pda::{find_pda, lookup_table_addresses};
use crate::errors::BuilderError;

/// Program id of the Address Lookup Table program.
pub const ADDRESS_LOOKUP_TABLE_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("AddressLookupTab1e1111111111111111111111111");

/// Size of the fixed header ("meta") of a table account, in bytes.
pub const LOOKUP_TABLE_META_SIZE: usize = 56;
/// A table holds at most 256 addresses.
pub const MAX_LOOKUP_TABLE_ADDRESSES: usize = 256;
/// Addresses added by one `ExtendLookupTable`. Conservative: it keeps the
/// extending transaction well inside a single packet.
pub const MAX_ADDRESSES_PER_EXTEND: usize = 20;
/// `deactivation_slot` of a table that is not being deactivated.
pub const NOT_DEACTIVATED: u64 = u64::MAX;

/// Instruction index of `CreateLookupTable`.
const CREATE_LOOKUP_TABLE: u32 = 0;
/// Instruction index of `ExtendLookupTable`.
const EXTEND_LOOKUP_TABLE: u32 = 2;
/// Account-state discriminator of an initialized table.
const STATE_LOOKUP_TABLE: u32 = 1;
const PUBKEY_BYTES: usize = 32;

fn invalid(reason: &'static str) -> BuilderError {
    BuilderError::InvalidLookupTable { reason }
}

/// Derives the address of the table created by `authority` at `recent_slot`
/// (seeds: `[authority, recent_slot as u64 LE]`) and its bump seed.
///
/// # Errors
/// [`BuilderError::PdaDerivationFailed`].
pub fn derive_lookup_table_address(
    authority: &Pubkey,
    recent_slot: u64,
) -> Result<(Pubkey, u8), BuilderError> {
    let authority_bytes = authority.to_bytes();
    let slot_bytes = recent_slot.to_le_bytes();
    find_pda(
        "lookup_table",
        &[authority_bytes.as_slice(), slot_bytes.as_slice()],
        &ADDRESS_LOOKUP_TABLE_PROGRAM_ID,
    )
}

/// `CreateLookupTable` (signed variant: the authority signs). Returns the
/// instruction together with the address of the table it creates.
///
/// `recent_slot` must be a slot that is still in the `SlotHashes` sysvar, i.e.
/// very recent.
///
/// # Errors
/// [`BuilderError::PdaDerivationFailed`].
pub fn create_lookup_table_instruction(
    authority: &Pubkey,
    payer: &Pubkey,
    recent_slot: u64,
) -> Result<(Instruction, Pubkey), BuilderError> {
    let (table, bump) = derive_lookup_table_address(authority, recent_slot)?;
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&CREATE_LOOKUP_TABLE.to_le_bytes());
    data.extend_from_slice(&recent_slot.to_le_bytes());
    data.push(bump);
    let instruction = Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    };
    Ok((instruction, table))
}

/// `ExtendLookupTable` appending `new_addresses` to `table`.
///
/// # Errors
/// [`BuilderError::InvalidLookupTable`] if the list is empty or longer than
/// [`MAX_ADDRESSES_PER_EXTEND`].
pub fn extend_lookup_table_instruction(
    table: &Pubkey,
    authority: &Pubkey,
    payer: &Pubkey,
    new_addresses: &[Pubkey],
) -> Result<Instruction, BuilderError> {
    if new_addresses.is_empty() {
        return Err(invalid("nothing to add to the table"));
    }
    if new_addresses.len() > MAX_ADDRESSES_PER_EXTEND {
        return Err(invalid("too many addresses for one extend instruction"));
    }
    let count = u64::try_from(new_addresses.len())
        .map_err(|_| invalid("address count does not fit in 64 bits"))?;
    let mut data = Vec::with_capacity(12 + PUBKEY_BYTES * new_addresses.len());
    data.extend_from_slice(&EXTEND_LOOKUP_TABLE.to_le_bytes());
    data.extend_from_slice(&count.to_le_bytes());
    for address in new_addresses {
        data.extend_from_slice(&address.to_bytes());
    }
    Ok(Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*table, false),
            AccountMeta::new_readonly(*authority, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
        ],
        data,
    })
}

/// The instructions that create the launch table and fill it with the static
/// launch addresses ([`lookup_table_addresses`]). Returns them together with the
/// table address. Send them in one transaction, wait for the next slot, then
/// use the table.
///
/// # Errors
/// See [`create_lookup_table_instruction`] and [`extend_lookup_table_instruction`].
pub fn launch_table_setup(
    authority: &Pubkey,
    payer: &Pubkey,
    recent_slot: u64,
) -> Result<(Vec<Instruction>, Pubkey), BuilderError> {
    let (create, table) = create_lookup_table_instruction(authority, payer, recent_slot)?;
    let mut instructions = vec![create];
    for chunk in lookup_table_addresses()?.chunks(MAX_ADDRESSES_PER_EXTEND) {
        instructions.push(extend_lookup_table_instruction(
            &table, authority, payer, chunk,
        )?);
    }
    Ok((instructions, table))
}

/// A table account as stored on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedLookupTable {
    /// Slot at which deactivation started, or [`NOT_DEACTIVATED`].
    pub deactivation_slot: u64,
    /// Slot of the most recent `ExtendLookupTable`.
    pub last_extended_slot: u64,
    /// Authority that may extend/close the table (`None` once frozen).
    pub authority: Option<Pubkey>,
    /// The stored addresses, in index order.
    pub addresses: Vec<Pubkey>,
}

impl DecodedLookupTable {
    /// `true` while the table is not being deactivated.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.deactivation_slot == NOT_DEACTIVATED
    }

    /// `true` when every address is already usable in `current_slot`: addresses
    /// become visible one slot after the extension that added them.
    #[must_use]
    pub fn is_usable_at(&self, current_slot: u64) -> bool {
        self.is_active() && self.last_extended_slot < current_slot
    }
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(<[u8; 4]>::try_from(bytes).ok()?))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(<[u8; 8]>::try_from(bytes).ok()?))
}

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    let bytes = data.get(offset..offset.checked_add(PUBKEY_BYTES)?)?;
    Some(Pubkey::new_from_array(
        <[u8; PUBKEY_BYTES]>::try_from(bytes).ok()?,
    ))
}

/// Decodes the raw data of a table account (`getAccountInfo`).
///
/// Layout: `u32` state (1 = table), `u64` deactivation slot, `u64` last
/// extended slot, `u8` start index, `Option<Pubkey>` authority (1 + 32 bytes),
/// 2 bytes of padding (56 bytes in total), followed by the 32-byte addresses.
///
/// # Errors
/// [`BuilderError::InvalidLookupTable`] for truncated or inconsistent data.
pub fn decode_lookup_table(data: &[u8]) -> Result<DecodedLookupTable, BuilderError> {
    if data.len() < LOOKUP_TABLE_META_SIZE {
        return Err(invalid("account data is shorter than the table header"));
    }
    if read_u32(data, 0) != Some(STATE_LOOKUP_TABLE) {
        return Err(invalid("account is not an initialized lookup table"));
    }
    let deactivation_slot = read_u64(data, 4).ok_or_else(|| invalid("truncated header"))?;
    let last_extended_slot = read_u64(data, 12).ok_or_else(|| invalid("truncated header"))?;
    let authority = match data.get(21).copied() {
        Some(0) => None,
        Some(1) => Some(read_pubkey(data, 22).ok_or_else(|| invalid("truncated header"))?),
        _ => return Err(invalid("malformed authority field")),
    };

    let body = data
        .get(LOOKUP_TABLE_META_SIZE..)
        .ok_or_else(|| invalid("truncated header"))?;
    if body.len() % PUBKEY_BYTES != 0 {
        return Err(invalid("address area is not a multiple of 32 bytes"));
    }
    let addresses: Vec<Pubkey> = body
        .chunks_exact(PUBKEY_BYTES)
        .filter_map(|chunk| <[u8; PUBKEY_BYTES]>::try_from(chunk).ok())
        .map(Pubkey::new_from_array)
        .collect();
    if addresses.len() > MAX_LOOKUP_TABLE_ADDRESSES {
        return Err(invalid("table holds more than 256 addresses"));
    }
    Ok(DecodedLookupTable {
        deactivation_slot,
        last_extended_slot,
        authority,
        addresses,
    })
}

/// A lookup table known to contain every static launch address.
///
/// Holding one is the proof the builder needs: with it, the compiled launch is
/// the compact (about 905 byte) form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchLookupTable {
    address: Pubkey,
    addresses: Vec<Pubkey>,
}

impl LaunchLookupTable {
    /// The table a correct [`launch_table_setup`] produces, by its address. Use
    /// it when the table was created by this program and is known to be
    /// confirmed; otherwise prefer [`LaunchLookupTable::from_decoded`].
    ///
    /// # Errors
    /// [`BuilderError::PdaDerivationFailed`].
    pub fn from_static(address: Pubkey) -> Result<Self, BuilderError> {
        Ok(Self {
            address,
            addresses: lookup_table_addresses()?,
        })
    }

    /// Builds the handle from the on-chain table, after checking that it is
    /// active, has warmed up at `current_slot` and holds every launch address.
    ///
    /// # Errors
    /// [`BuilderError::InvalidLookupTable`].
    pub fn from_decoded(
        address: Pubkey,
        decoded: &DecodedLookupTable,
        current_slot: u64,
    ) -> Result<Self, BuilderError> {
        if !decoded.is_active() {
            return Err(invalid("table is being deactivated"));
        }
        if !decoded.is_usable_at(current_slot) {
            return Err(invalid("table was extended in the current slot"));
        }
        let table = Self {
            address,
            addresses: decoded.addresses.clone(),
        };
        if !table.covers_launch()? {
            return Err(invalid("table is missing launch addresses"));
        }
        Ok(table)
    }

    /// Address of the table account.
    #[must_use]
    pub fn address(&self) -> Pubkey {
        self.address
    }

    /// Addresses stored in the table, in index order.
    #[must_use]
    pub fn addresses(&self) -> &[Pubkey] {
        &self.addresses
    }

    /// `true` if every address of [`lookup_table_addresses`] is present.
    ///
    /// # Errors
    /// [`BuilderError::PdaDerivationFailed`].
    pub fn covers_launch(&self) -> Result<bool, BuilderError> {
        Ok(lookup_table_addresses()?
            .iter()
            .all(|wanted| self.addresses.contains(wanted)))
    }

    /// The form the message compiler consumes.
    #[must_use]
    pub fn to_account(&self) -> AddressLookupTableAccount {
        AddressLookupTableAccount {
            key: self.address,
            addresses: self.addresses.clone(),
        }
    }
}
