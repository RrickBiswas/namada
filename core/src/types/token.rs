//! A basic fungible token

use std::fmt::{Display, Formatter};
use std::ops::{Add, AddAssign, Div, Mul, Sub, SubAssign};
use std::str::FromStr;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use data_encoding::BASE32HEX_NOPAD;
use masp_primitives::transaction::Transaction;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::dec::POS_DECIMAL_PRECISION;
use crate::ibc::applications::transfer::Amount as IbcAmount;
use crate::ledger::storage_api::token::read_denom;
use crate::ledger::storage_api::StorageRead;
use crate::types::address::{masp, Address, DecodeError as AddressError};
use crate::types::dec::Dec;
use crate::types::storage;
use crate::types::storage::{DbKeySeg, Key, KeySeg};
use crate::types::uint::{self, Uint, I256};

/// Amount in micro units. For different granularity another representation
/// might be more appropriate.
#[derive(
    Clone,
    Copy,
    Default,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    Hash,
)]
pub struct Amount {
    raw: Uint,
}

/// Maximum decimal places in a native token [`Amount`] and [`Change`].
/// For non-native (e.g. ERC20 tokens) one must read the `denom_key` storage
/// key.
pub const NATIVE_MAX_DECIMAL_PLACES: u8 = 6;

/// Decimal scale of a native token [`Amount`] and [`Change`].
/// For non-native (e.g. ERC20 tokens) one must read the `denom_key` storage
/// key.
pub const NATIVE_SCALE: u64 = 1_000_000;

/// A change in tokens amount
pub type Change = I256;

impl Amount {
    /// Get the amount as a [`Change`]
    pub fn change(&self) -> Change {
        self.raw.try_into().unwrap()
    }

    /// Spend a given amount.
    /// Panics when given `amount` > `self.raw` amount.
    pub fn spend(&mut self, amount: &Amount) {
        self.raw = self.raw.checked_sub(amount.raw).unwrap();
    }

    /// Receive a given amount.
    /// Panics on overflow and when [`uint::MAX_SIGNED_VALUE`] is exceeded.
    pub fn receive(&mut self, amount: &Amount) {
        self.raw = self.raw.checked_add(amount.raw).unwrap();
    }

    /// Create a new amount of native token from whole number of tokens
    pub fn native_whole(amount: u64) -> Self {
        Self {
            raw: Uint::from(amount) * NATIVE_SCALE,
        }
    }

    /// Get the raw [`Uint`] value, which represents namnam
    pub fn raw_amount(&self) -> Uint {
        self.raw
    }

    /// Create a new amount with the maximum value
    pub fn max() -> Self {
        Self {
            raw: uint::MAX_VALUE,
        }
    }

    /// Create a new amount with the maximum signed value
    pub fn max_signed() -> Self {
        Self {
            raw: uint::MAX_SIGNED_VALUE,
        }
    }

    /// Zero [`Amount`].
    pub fn zero() -> Self {
        Self::default()
    }

    /// Check if [`Amount`] is zero.
    pub fn is_zero(&self) -> bool {
        self.raw == Uint::from(0)
    }

    /// Checked addition. Returns `None` on overflow or if
    /// the amount exceed [`uint::MAX_VALUE`]
    pub fn checked_add(&self, amount: Amount) -> Option<Self> {
        self.raw.checked_add(amount.raw).and_then(|result| {
            if result <= uint::MAX_VALUE {
                Some(Self { raw: result })
            } else {
                None
            }
        })
    }

    /// Checked addition. Returns `None` on overflow or if
    /// the amount exceed [`uint::MAX_SIGNED_VALUE`]
    pub fn checked_signed_add(&self, amount: Amount) -> Option<Self> {
        self.raw.checked_add(amount.raw).and_then(|result| {
            if result <= uint::MAX_SIGNED_VALUE {
                Some(Self { raw: result })
            } else {
                None
            }
        })
    }

    /// Checked subtraction. Returns `None` on underflow
    pub fn checked_sub(&self, amount: Amount) -> Option<Self> {
        self.raw
            .checked_sub(amount.raw)
            .map(|result| Self { raw: result })
    }

    /// Create amount from the absolute value of `Change`.
    pub fn from_change(change: Change) -> Self {
        Self { raw: change.abs() }
    }

    /// Given a string and a denomination, parse an amount from string.
    pub fn from_str(
        string: impl AsRef<str>,
        denom: impl Into<u8>,
    ) -> Result<Amount, AmountParseError> {
        DenominatedAmount::from_str(string.as_ref())?
            .increase_precision(denom.into().into())
            .map(Into::into)
    }

    /// Attempt to convert an unsigned integer to an `Amount` with the
    /// specified precision.
    pub fn from_uint(
        uint: impl Into<Uint>,
        denom: impl Into<u8>,
    ) -> Result<Self, AmountParseError> {
        let denom = denom.into();
        match Uint::from(10)
            .checked_pow(Uint::from(denom))
            .and_then(|scaling| scaling.checked_mul(uint.into()))
        {
            Some(amount) => Ok(Self { raw: amount }),
            None => Err(AmountParseError::ConvertToDecimal),
        }
    }

    /// Given a u64 and [`MaspDenom`], construct the corresponding
    /// amount.
    pub fn from_masp_denominated(val: u64, denom: MaspDenom) -> Self {
        let mut raw = [0u64; 4];
        raw[denom as usize] = val;
        Self { raw: Uint(raw) }
    }

    /// Get a string representation of a native token amount.
    pub fn to_string_native(&self) -> String {
        DenominatedAmount {
            amount: *self,
            denom: NATIVE_MAX_DECIMAL_PLACES.into(),
        }
        .to_string_precise()
    }

    /// Add denomination info if it exists in storage.
    pub fn denominated(
        &self,
        token: &Address,
        sub_prefix: Option<&Key>,
        storage: &impl StorageRead,
    ) -> Option<DenominatedAmount> {
        let denom = read_denom(storage, token, sub_prefix)
            .expect("Should be able to read storage");
        denom.map(|denom| DenominatedAmount {
            amount: *self,
            denom,
        })
    }

    /// Convert to an [`Amount`] under the assumption that the input
    /// string encodes all necessary decimal places.
    pub fn from_string_precise(string: &str) -> Result<Self, AmountParseError> {
        DenominatedAmount::from_str(string).map(|den| den.amount)
    }
}

/// Given a number represented as `M*B^D`, then
/// `M` is the matissa, `B` is the base and `D`
/// is the denomination, represented by this stuct.
#[derive(
    Debug,
    Copy,
    Clone,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
#[serde(transparent)]
pub struct Denomination(pub u8);

impl From<u8> for Denomination {
    fn from(denom: u8) -> Self {
        Self(denom)
    }
}

impl From<Denomination> for u8 {
    fn from(denom: Denomination) -> Self {
        denom.0
    }
}

/// An amount with its denomination.
#[derive(
    Debug,
    Copy,
    Clone,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
)]
pub struct DenominatedAmount {
    /// The mantissa
    pub amount: Amount,
    /// The number of decimal places in base ten.
    pub denom: Denomination,
}

impl DenominatedAmount {
    /// A precise string representation. The number of
    /// decimal places in this string gives the denomination.
    /// This not true of the string produced by the `Display`
    /// trait.
    pub fn to_string_precise(&self) -> String {
        let decimals = self.denom.0 as usize;
        let mut string = self.amount.raw.to_string();
        if string.len() > decimals {
            string.insert(string.len() - decimals, '.');
        } else {
            for _ in string.len()..decimals {
                string.insert(0, '0');
            }
            string.insert(0, '.');
            string.insert(0, '0');
        }
        string
    }

    /// Find the minimal precision that holds this value losslessly.
    /// This equates to stripping trailing zeros after the decimal
    /// place.
    pub fn canonical(self) -> Self {
        let mut value = self.amount.raw;
        let ten = Uint::from(10);
        let mut denom = self.denom.0;
        for _ in 0..self.denom.0 {
            let (div, rem) = value.div_mod(ten);
            if rem == Uint::zero() {
                value = div;
                denom -= 1;
            }
        }
        Self {
            amount: Amount { raw: value },
            denom: denom.into(),
        }
    }

    /// Attempt to increase the precision of an amount. Can fail
    /// if the resulting amount does not fit into 256 bits.
    pub fn increase_precision(
        self,
        denom: Denomination,
    ) -> Result<Self, AmountParseError> {
        if denom.0 < self.denom.0 {
            return Err(AmountParseError::PrecisionDecrease);
        }
        Uint::from(10)
            .checked_pow(Uint::from(denom.0 - self.denom.0))
            .and_then(|scaling| self.amount.raw.checked_mul(scaling))
            .map(|amount| Self {
                amount: Amount { raw: amount },
                denom,
            })
            .ok_or(AmountParseError::PrecisionOverflow)
    }
}

impl Display for DenominatedAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = self.to_string_precise();
        let string = string.trim_end_matches(&['0']);
        let string = string.trim_end_matches(&['.']);
        f.write_str(string)
    }
}

impl FromStr for DenominatedAmount {
    type Err = AmountParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let precision = s.find('.').map(|pos| s.len() - pos - 1);
        let digits = s
            .chars()
            .filter_map(|c| {
                if c.is_numeric() {
                    c.to_digit(10).map(Uint::from)
                } else {
                    None
                }
            })
            .rev()
            .collect::<Vec<_>>();
        if digits.len() != s.len() && precision.is_none()
            || digits.len() != s.len() - 1 && precision.is_some()
        {
            return Err(AmountParseError::NotNumeric);
        }
        if digits.len() > 77 {
            return Err(AmountParseError::ScaleTooLarge(
                digits.len() as u32,
                77,
            ));
        }
        let mut value = Uint::default();
        let ten = Uint::from(10);
        for (pow, digit) in digits.into_iter().enumerate() {
            value = ten
                .checked_pow(Uint::from(pow))
                .and_then(|scaling| scaling.checked_mul(digit))
                .and_then(|scaled| value.checked_add(scaled))
                .ok_or(AmountParseError::InvalidRange)?;
        }
        let denom = Denomination(precision.unwrap_or_default() as u8);
        Ok(Self {
            amount: Amount { raw: value },
            denom,
        })
    }
}

impl serde::Serialize for Amount {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let amount_string = self.raw.to_string();
        serde::Serialize::serialize(&amount_string, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for Amount {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let amount_string: String =
            serde::Deserialize::deserialize(deserializer)?;
        Ok(Self {
            raw: Uint::from_str(&amount_string).map_err(D::Error::custom)?,
        })
    }
}

impl serde::Serialize for DenominatedAmount {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let amount_string = self.to_string_precise();
        serde::Serialize::serialize(&amount_string, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for DenominatedAmount {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let amount_string: String =
            serde::Deserialize::deserialize(deserializer)?;
        Self::from_str(&amount_string).map_err(D::Error::custom)
    }
}

impl<'a> From<&'a DenominatedAmount> for &'a Amount {
    fn from(denom: &'a DenominatedAmount) -> Self {
        &denom.amount
    }
}

impl From<DenominatedAmount> for Amount {
    fn from(denom: DenominatedAmount) -> Self {
        denom.amount
    }
}

// Treats the u64 as a value of the raw amount (namnam)
impl From<u64> for Amount {
    fn from(val: u64) -> Amount {
        Amount {
            raw: Uint::from(val),
        }
    }
}

impl From<Dec> for Amount {
    fn from(dec: Dec) -> Amount {
        if !dec.is_negative() {
            Amount {
                raw: dec.0.abs() / Uint::exp10(POS_DECIMAL_PRECISION as usize),
            }
        } else {
            panic!(
                "The Dec value is negative and cannot be multiplied by an \
                 Amount"
            )
        }
    }
}

impl TryFrom<Amount> for u128 {
    type Error = std::io::Error;

    fn try_from(value: Amount) -> Result<Self, Self::Error> {
        let Uint(arr) = value.raw;
        for word in arr.iter().skip(2) {
            if *word != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Integer overflow when casting to u128",
                ));
            }
        }
        Ok(value.raw.low_u128())
    }
}

impl Add for Amount {
    type Output = Amount;

    fn add(mut self, rhs: Self) -> Self::Output {
        self.raw += rhs.raw;
        self
    }
}

impl Add<u64> for Amount {
    type Output = Self;

    fn add(self, rhs: u64) -> Self::Output {
        Self {
            raw: self.raw + Uint::from(rhs),
        }
    }
}

impl std::iter::Sum for Amount {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Amount::zero(), |acc, amt| acc + amt)
    }
}

impl Mul<u64> for Amount {
    type Output = Amount;

    fn mul(mut self, rhs: u64) -> Self::Output {
        self.raw *= rhs;
        self
    }
}

impl Mul<Uint> for Amount {
    type Output = Amount;

    fn mul(mut self, rhs: Uint) -> Self::Output {
        self.raw *= rhs;
        self
    }
}

impl Mul<Amount> for Amount {
    type Output = Amount;

    fn mul(mut self, rhs: Amount) -> Self::Output {
        self.raw *= rhs.raw;
        self
    }
}

/// A combination of Euclidean division and fractions:
/// x*(a,b) = (a*(x//b), x%b).
impl Mul<(u64, u64)> for Amount {
    type Output = (Amount, Amount);

    fn mul(mut self, rhs: (u64, u64)) -> Self::Output {
        let amt = Amount {
            raw: (self.raw / rhs.1) * rhs.0,
        };
        self.raw %= rhs.1;
        (amt, self)
    }
}

impl Div<u64> for Amount {
    type Output = Self;

    fn div(self, rhs: u64) -> Self::Output {
        Self {
            raw: self.raw / Uint::from(rhs),
        }
    }
}

impl AddAssign for Amount {
    fn add_assign(&mut self, rhs: Self) {
        self.raw += rhs.raw
    }
}

impl Sub for Amount {
    type Output = Amount;

    fn sub(mut self, rhs: Self) -> Self::Output {
        self.raw -= rhs.raw;
        self
    }
}

impl SubAssign for Amount {
    fn sub_assign(&mut self, rhs: Self) {
        self.raw -= rhs.raw
    }
}

impl KeySeg for Amount {
    fn parse(string: String) -> super::storage::Result<Self>
    where
        Self: Sized,
    {
        let bytes = BASE32HEX_NOPAD.decode(string.as_ref()).map_err(|err| {
            storage::Error::ParseKeySeg(format!(
                "Failed parsing {} with {}",
                string, err
            ))
        })?;
        Ok(Amount {
            raw: Uint::from_big_endian(&bytes),
        })
    }

    fn raw(&self) -> String {
        let mut buf = [0u8; 32];
        self.raw.to_big_endian(&mut buf);
        BASE32HEX_NOPAD.encode(&buf)
    }

    fn to_db_key(&self) -> DbKeySeg {
        DbKeySeg::StringSeg(self.raw())
    }
}

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum AmountParseError {
    #[error(
        "Error decoding token amount, too many decimal places: {0}. Maximum \
         {1}"
    )]
    ScaleTooLarge(u32, u8),
    #[error(
        "Error decoding token amount, the value is not within invalid range."
    )]
    InvalidRange,
    #[error("Error converting amount to decimal, number too large.")]
    ConvertToDecimal,
    #[error(
        "Could not convert from string, expected an unsigned 256-bit integer."
    )]
    FromString,
    #[error("Could not parse string as a correctly formatted number.")]
    NotNumeric,
    #[error("This amount cannot handle the requested precision in 256 bits.")]
    PrecisionOverflow,
    #[error("More precision given in the amount than requested.")]
    PrecisionDecrease,
}

impl From<Amount> for Change {
    fn from(amount: Amount) -> Self {
        amount.raw.try_into().unwrap()
    }
}

impl From<Change> for Amount {
    fn from(change: Change) -> Self {
        Amount { raw: change.abs() }
    }
}

impl From<Amount> for Uint {
    fn from(amount: Amount) -> Self {
        amount.raw
    }
}

/// The four possible u64 words in a [`Uint`].
/// Used for converting to MASP amounts.
#[derive(
    Copy,
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    BorshSerialize,
    BorshDeserialize,
)]
#[repr(u8)]
#[allow(missing_docs)]
pub enum MaspDenom {
    Zero = 0,
    One,
    Two,
    Three,
}

impl From<u8> for MaspDenom {
    fn from(denom: u8) -> Self {
        match denom {
            0 => Self::Zero,
            1 => Self::One,
            2 => Self::Two,
            3 => Self::Three,
            _ => panic!("Possible MASP denominations must be between 0 and 3"),
        }
    }
}

impl MaspDenom {
    /// Iterator over the possible denominations
    pub fn iter() -> impl Iterator<Item = MaspDenom> {
        (0u8..3).map(Self::from)
    }

    /// Get the corresponding u64 word from the input uint256.
    pub fn denominate<'a>(&self, amount: impl Into<&'a Amount>) -> u64 {
        let amount = amount.into();
        amount.raw.0[*self as usize]
    }

    /// Get the corresponding u64 word from the input uint256.
    pub fn denominate_i64(&self, amount: &Change) -> i64 {
        let val = amount.abs().0[*self as usize] as i64;
        if Change::is_negative(amount) {
            -val
        } else {
            val
        }
    }
}

impl TryFrom<IbcAmount> for Amount {
    type Error = AmountParseError;

    fn try_from(amount: IbcAmount) -> Result<Self, Self::Error> {
        // TODO: https://github.com/anoma/namada/issues/1089
        // TODO: OVERFLOW CHECK PLEASE (PATCH IBC TO ALLOW GETTING
        // IBCAMOUNT::MAX OR SIMILAR) if amount > u64::MAX.into() {
        //    return Err(AmountParseError::InvalidRange);
        //}
        DenominatedAmount::from_str(&amount.to_string()).map(|a| a.amount)
    }
}

/// Key segment for a balance key
pub const BALANCE_STORAGE_KEY: &str = "balance";
/// Key segment for a denomination key
pub const DENOM_STORAGE_KEY: &str = "denom";
/// Key segment for head shielded transaction pointer keys
pub const HEAD_TX_KEY: &str = "head-tx";
/// Key segment prefix for shielded transaction key
pub const TX_KEY_PREFIX: &str = "tx-";
/// Key segment prefix for MASP conversions
pub const CONVERSION_KEY_PREFIX: &str = "conv";
/// Key segment prefix for pinned shielded transactions
pub const PIN_KEY_PREFIX: &str = "pin-";
const TOTAL_SUPPLY_STORAGE_KEY: &str = "total_supply";

/// A fully qualified (multi-) token address.
#[derive(
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Debug,
    Hash,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct TokenAddress {
    /// The address of the (multi-) token
    pub address: Address,
    /// If it is a mutli-token, this indicates the sub-token.
    pub sub_prefix: Option<Key>,
}

impl TokenAddress {
    /// A function for displaying a [`TokenAddress`]. Takes a
    /// human readable name of the token as input.
    pub fn format_with_alias(&self, alias: &str) -> String {
        format!(
            "{}{}",
            alias,
            self.sub_prefix
                .as_ref()
                .map(|k| format!("/{}", k))
                .unwrap_or_default()
        )
    }
}

impl Display for TokenAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let formatted = format!(
            "{}{}",
            self.address,
            self.sub_prefix
                .as_ref()
                .map(|k| format!("/{}", k))
                .unwrap_or_default()
        );
        f.write_str(&formatted)
    }
}

/// Obtain a storage key for user's balance.
pub fn balance_key(token_addr: &Address, owner: &Address) -> Key {
    Key::from(token_addr.to_db_key())
        .push(&BALANCE_STORAGE_KEY.to_owned())
        .expect("Cannot obtain a storage key")
        .push(&owner.to_db_key())
        .expect("Cannot obtain a storage key")
}

/// Obtain a storage key prefix for all users' balances.
pub fn balance_prefix(token_addr: &Address) -> Key {
    Key::from(token_addr.to_db_key())
        .push(&BALANCE_STORAGE_KEY.to_owned())
        .expect("Cannot obtain a storage key")
}

/// Obtain a storage key prefix for multitoken balances.
pub fn multitoken_balance_prefix(
    token_addr: &Address,
    sub_prefix: &Key,
) -> Key {
    Key::from(token_addr.to_db_key()).join(sub_prefix)
}

/// Obtain a storage key for user's multitoken balance.
pub fn multitoken_balance_key(prefix: &Key, owner: &Address) -> Key {
    prefix
        .push(&BALANCE_STORAGE_KEY.to_owned())
        .expect("Cannot obtain a storage key")
        .push(&owner.to_db_key())
        .expect("Cannot obtain a storage key")
}

/// Check if the given storage key is balance key for the given token. If it is,
/// returns the owner.
pub fn is_balance_key<'a>(
    token_addr: &Address,
    key: &'a Key,
) -> Option<&'a Address> {
    match &key.segments[..] {
        [
            DbKeySeg::AddressSeg(addr),
            DbKeySeg::StringSeg(key),
            DbKeySeg::AddressSeg(owner),
        ] if key == BALANCE_STORAGE_KEY && addr == token_addr => Some(owner),
        _ => None,
    }
}

/// Check if the given storage key is balance key for unspecified token. If it
/// is, returns the token and owner address.
pub fn is_any_token_balance_key(key: &Key) -> Option<[&Address; 2]> {
    match &key.segments[..] {
        [
            DbKeySeg::AddressSeg(token),
            DbKeySeg::StringSeg(key),
            DbKeySeg::AddressSeg(owner),
        ] if key == BALANCE_STORAGE_KEY => Some([token, owner]),
        _ => None,
    }
}

/// Obtain a storage key denomination of a token.
pub fn denom_key(token_addr: &Address, sub_prefix: Option<&Key>) -> Key {
    match sub_prefix {
        Some(sub) => Key::from(token_addr.to_db_key())
            .join(sub)
            .push(&DENOM_STORAGE_KEY.to_owned())
            .expect("Cannot obtain a storage key"),
        None => Key::from(token_addr.to_db_key())
            .push(&DENOM_STORAGE_KEY.to_owned())
            .expect("Cannot obtain a storage key"),
    }
}

/// Check if the given storage key is a denomination key for the given token.
pub fn is_denom_key(token_addr: &Address, key: &Key) -> bool {
    matches!(&key.segments[..],
        [
            DbKeySeg::AddressSeg(addr),
            ..,
            DbKeySeg::StringSeg(key),
        ] if key == DENOM_STORAGE_KEY && addr == token_addr)
}

/// Check if the given storage key is a masp key
pub fn is_masp_key(key: &Key) -> bool {
    matches!(&key.segments[..],
        [DbKeySeg::AddressSeg(addr), DbKeySeg::StringSeg(key)]
            if *addr == masp()
                && (key == HEAD_TX_KEY
                    || key.starts_with(TX_KEY_PREFIX)
                    || key.starts_with(PIN_KEY_PREFIX)))
}

/// Storage key for total supply of a token
pub fn total_supply_key(token_address: &Address) -> Key {
    Key::from(token_address.to_db_key())
        .push(&TOTAL_SUPPLY_STORAGE_KEY.to_owned())
        .expect("Cannot obtain a storage key")
}

/// Is storage key for total supply of a specific token?
pub fn is_total_supply_key(key: &Key, token_address: &Address) -> bool {
    matches!(&key.segments[..], [DbKeySeg::AddressSeg(addr), DbKeySeg::StringSeg(key)] if addr == token_address && key == TOTAL_SUPPLY_STORAGE_KEY)
}

/// Check if the given storage key is multitoken balance key for the given
/// token. If it is, returns the sub prefix and the owner.
pub fn is_multitoken_balance_key<'a>(
    token_addr: &Address,
    key: &'a Key,
) -> Option<(Key, &'a Address)> {
    match key.segments.first() {
        Some(DbKeySeg::AddressSeg(addr)) if addr == token_addr => {
            multitoken_balance_owner(key)
        }
        _ => None,
    }
}

/// Check if the given storage key is multitoken balance key for unspecified
/// token. If it is, returns the sub prefix and the token and owner addresses.
pub fn is_any_multitoken_balance_key(
    key: &Key,
) -> Option<(Key, [&Address; 2])> {
    match key.segments.first() {
        Some(DbKeySeg::AddressSeg(token)) => multitoken_balance_owner(key)
            .map(|(sub, owner)| (sub, [token, owner])),
        _ => None,
    }
}

pub fn is_any_token_or_multitoken_balance_key(
    key: &Key,
) -> Option<[&Address; 2]> {
    is_any_multitoken_balance_key(key)
        .map(|a| a.1)
        .or_else(|| is_any_token_balance_key(key))
}

fn multitoken_balance_owner(key: &Key) -> Option<(Key, &Address)> {
    let len = key.segments.len();
    if len < 4 {
        // the key of a multitoken should have 1 or more segments other than
        // token, balance, owner
        return None;
    }
    match &key.segments[..] {
        [
            ..,
            DbKeySeg::StringSeg(balance),
            DbKeySeg::AddressSeg(owner),
        ] if balance == BALANCE_STORAGE_KEY => {
            let sub_prefix = Key {
                segments: key.segments[1..(len - 2)].to_vec(),
            };
            Some((sub_prefix, owner))
        }
        _ => None,
    }
}

/// A simple bilateral token transfer
#[derive(
    Debug,
    Clone,
    PartialEq,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Hash,
    Eq,
    PartialOrd,
    Serialize,
    Deserialize,
)]
pub struct Transfer {
    /// Source address will spend the tokens
    pub source: Address,
    /// Target address will receive the tokens
    pub target: Address,
    /// Token's address
    pub token: Address,
    /// Source token's sub prefix
    pub sub_prefix: Option<Key>,
    /// The amount of tokens
    pub amount: DenominatedAmount,
    /// The unused storage location at which to place TxId
    pub key: Option<String>,
    /// Shielded transaction part
    pub shielded: Option<Transaction>,
}

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum TransferError {
    #[error("Invalid address is specified: {0}")]
    Address(AddressError),
    #[error("Invalid amount: {0}")]
    Amount(AmountParseError),
    #[error("No token is specified")]
    NoToken,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_display() {
        let max = Amount::from_uint(u64::MAX, 0).expect("Test failed");
        assert_eq!("18446744073709.551615", max.to_string_native());
        let max = DenominatedAmount {
            amount: max,
            denom: NATIVE_MAX_DECIMAL_PLACES.into(),
        };
        assert_eq!("18446744073709.551615", max.to_string());

        let whole =
            Amount::from_uint(u64::MAX / NATIVE_SCALE * NATIVE_SCALE, 0)
                .expect("Test failed");
        assert_eq!("18446744073709.000000", whole.to_string_native());
        let whole = DenominatedAmount {
            amount: whole,
            denom: NATIVE_MAX_DECIMAL_PLACES.into(),
        };
        assert_eq!("18446744073709", whole.to_string());

        let trailing_zeroes =
            Amount::from_uint(123000, 0).expect("Test failed");
        assert_eq!("0.123000", trailing_zeroes.to_string_native());
        let trailing_zeroes = DenominatedAmount {
            amount: trailing_zeroes,
            denom: NATIVE_MAX_DECIMAL_PLACES.into(),
        };
        assert_eq!("0.123", trailing_zeroes.to_string());

        let zero = Amount::default();
        assert_eq!("0.000000", zero.to_string_native());
        let zero = DenominatedAmount {
            amount: zero,
            denom: NATIVE_MAX_DECIMAL_PLACES.into(),
        };
        assert_eq!("0", zero.to_string());

        let amount = DenominatedAmount {
            amount: Amount::from_uint(1120, 0).expect("Test failed"),
            denom: 3u8.into(),
        };
        assert_eq!("1.12", amount.to_string());
        assert_eq!("1.120", amount.to_string_precise());

        let amount = DenominatedAmount {
            amount: Amount::from_uint(1120, 0).expect("Test failed"),
            denom: 5u8.into(),
        };
        assert_eq!("0.0112", amount.to_string());
        assert_eq!("0.01120", amount.to_string_precise());
    }

    #[test]
    fn test_amount_checked_sub() {
        let max = Amount::native_whole(u64::MAX);
        let one = Amount::native_whole(1);
        let zero = Amount::native_whole(0);

        assert_eq!(zero.checked_sub(zero), Some(zero));
        assert_eq!(zero.checked_sub(one), None);
        assert_eq!(zero.checked_sub(max), None);

        assert_eq!(max.checked_sub(zero), Some(max));
        assert_eq!(max.checked_sub(one), Some(max - one));
        assert_eq!(max.checked_sub(max), Some(zero));
    }

    #[test]
    fn test_amount_checked_add() {
        let max = Amount::max();
        let max_signed = Amount::max_signed();
        let one = Amount::native_whole(1);
        let zero = Amount::native_whole(0);

        assert_eq!(zero.checked_add(zero), Some(zero));
        assert_eq!(zero.checked_signed_add(zero), Some(zero));
        assert_eq!(zero.checked_add(one), Some(one));
        assert_eq!(zero.checked_add(max - one), Some(max - one));
        assert_eq!(
            zero.checked_signed_add(max_signed - one),
            Some(max_signed - one)
        );
        assert_eq!(zero.checked_add(max), Some(max));
        assert_eq!(zero.checked_signed_add(max_signed), Some(max_signed));

        assert_eq!(max.checked_add(zero), Some(max));
        assert_eq!(max.checked_signed_add(zero), None);
        assert_eq!(max.checked_add(one), None);
        assert_eq!(max.checked_add(max), None);

        assert_eq!(max_signed.checked_add(zero), Some(max_signed));
        assert_eq!(max_signed.checked_add(one), Some(max_signed + one));
        assert_eq!(max_signed.checked_signed_add(max_signed), None);
    }

    #[test]
    fn test_amount_from_string() {
        assert!(Amount::from_str("1.12", 1).is_err());
        assert!(Amount::from_str("0.0", 0).is_err());
        assert!(Amount::from_str("1.12", 80).is_err());
        assert!(Amount::from_str("1.12.1", 3).is_err());
        assert!(Amount::from_str("1.1a", 3).is_err());
        assert_eq!(
            Amount::zero(),
            Amount::from_str("0.0", 1).expect("Test failed")
        );
        assert_eq!(
            Amount::zero(),
            Amount::from_str(".0", 1).expect("Test failed")
        );

        let amount = Amount::from_str("1.12", 3).expect("Test failed");
        assert_eq!(amount, Amount::from_uint(1120, 0).expect("Test failed"));
        let amount = Amount::from_str(".34", 3).expect("Test failed");
        assert_eq!(amount, Amount::from_uint(340, 0).expect("Test failed"));
        let amount = Amount::from_str("0.34", 3).expect("Test failed");
        assert_eq!(amount, Amount::from_uint(340, 0).expect("Test failed"));
        let amount = Amount::from_str("34", 1).expect("Test failed");
        assert_eq!(amount, Amount::from_uint(340, 0).expect("Test failed"));
    }

    #[test]
    fn test_from_masp_denominated() {
        let uint = Uint([15u64, 16, 17, 18]);
        let original = Amount::from_uint(uint, 0).expect("Test failed");
        for denom in MaspDenom::iter() {
            let word = denom.denominate(&original);
            assert_eq!(word, denom as u64 + 15u64);
            let amount = Amount::from_masp_denominated(word, denom);
            let raw = Uint::from(amount).0;
            let mut expected = [0u64; 4];
            expected[denom as usize] = word;
            assert_eq!(raw, expected);
        }
    }

    #[test]
    fn test_key_seg() {
        let original = Amount::from_uint(1234560000, 0).expect("Test failed");
        let key = original.raw();
        let amount = Amount::parse(key).expect("Test failed");
        assert_eq!(amount, original);
    }

    #[test]
    fn test_amount_is_zero() {
        let zero = Amount::zero();
        assert!(zero.is_zero());

        let non_zero = Amount::from_uint(1, 0).expect("Test failed");
        assert!(!non_zero.is_zero());
    }
}

/// Helpers for testing with addresses.
#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use proptest::prelude::*;

    use super::*;

    /// Generate an arbitrary token amount
    pub fn arb_amount() -> impl Strategy<Value = Amount> {
        any::<u64>().prop_map(|val| Amount::from_uint(val, 0).unwrap())
    }

    /// Generate an arbitrary token amount up to and including given `max` value
    pub fn arb_amount_ceiled(max: u64) -> impl Strategy<Value = Amount> {
        (0..=max).prop_map(|val| Amount::from_uint(val, 0).unwrap())
    }

    /// Generate an arbitrary non-zero token amount up to and including given
    /// `max` value
    pub fn arb_amount_non_zero_ceiled(
        max: u64,
    ) -> impl Strategy<Value = Amount> {
        (1..=max).prop_map(|val| Amount::from_uint(val, 0).unwrap())
    }
}
