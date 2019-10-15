use crate::*;
use crc16::*;
use ed25519_dalek::{Keypair, PublicKey};
use std::convert::Into;
use std::convert::TryFrom;
use std::io::{Cursor, Read, Seek};
use std::sync::{Arc, Mutex};
use tvm::block::{
    Account, AccountState, CurrencyCollection, Deserializable, ExternalInboundMessageHeader,
    GetRepresentationHash, Grams, Message as TvmMessage, MessageId, MsgAddressInt,
    Serializable, StateInit, TransactionId, TransactionProcessingStatus,
};
use tvm::cells_serialization::{deserialize_cells_tree, BagOfCells};
use tvm::stack::dictionary::{HashmapE, HashmapType};
use tvm::stack::{BuilderData, CellData, IBitstring, SliceData};
use tvm::types::AccountId;

pub use ton_abi::json_abi::DecodedMessage;
pub use ton_abi::token::{Token, TokenValue, Tokenizer};

#[cfg(feature = "node_interaction")]
use futures::stream::Stream;

#[cfg(feature = "node_interaction")]
const ACCOUNT_FIELDS: &str = r#"
    id
    storage {
        balance {
            Grams
        }
        state {
            ...on AccountStorageStateAccountUninitVariant {
                AccountUninit {
                    None
                }
            }
            ...on AccountStorageStateAccountActiveVariant {
                AccountActive {
                    code
                    data
                }
            }
            ...on AccountStorageStateAccountFrozenVariant {
                AccountFrozen {
                    None
                }
            }
        }
    }
"#;

lazy_static! {
    static ref DEFAULT_WORKCHAIN: Mutex<Option<i32>> = Mutex::new(None);
}

#[cfg(test)]
#[path = "tests/test_contract.rs"]
mod tests;

// The struct represents status of message that performs contract's call
#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct ContractCallState {
    pub id: TransactionId,
    pub status: TransactionProcessingStatus,
}

// The struct represents conract's image
pub struct ContractImage {
    state_init: StateInit,
    id: AccountId
}

#[allow(dead_code)]
impl ContractImage {

    // Creating contract image from code data and library bags of cells
    pub fn new<T>(code: &mut T, data: Option<&mut T>, library: Option<&mut T>) -> SdkResult<Self>
        where T: Read + Seek {

        let mut state_init = StateInit::default();

        let mut code_roots = deserialize_cells_tree(code)?;
        if code_roots.len() != 1 {
            bail!(SdkErrorKind::InvalidData("Invalid code's bag of cells".into()));
        }
        state_init.set_code(code_roots.remove(0));

        if let Some(data_) = data {
            let mut data_roots = deserialize_cells_tree(data_)?;
            if data_roots.len() != 1 {
                bail!(SdkErrorKind::InvalidData("Invalid data's bag of cells".into()));
            }
            state_init.set_data(data_roots.remove(0));
        }

        if let Some(library_) = library {
            let mut library_roots = deserialize_cells_tree(library_)?;
            if library_roots.len() != 1 {
                bail!(SdkErrorKind::InvalidData("Invalid library's bag of cells".into()));
            }
            state_init.set_library(library_roots.remove(0));
        }

        let id = state_init.hash()?.into();

        Ok(Self{ state_init, id })
    }

    pub fn from_state_init_and_key<T>(state_init_bag: &mut T, pub_key: &PublicKey) -> SdkResult<Self>
        where T: Read + Seek {

        let mut si_roots = deserialize_cells_tree(state_init_bag)?;
        if si_roots.len() != 1 {
            bail!(SdkErrorKind::InvalidData("Invalid state init's bag of cells".into()));
        }

        let mut state_init : StateInit
            = StateInit::construct_from(&mut SliceData::from(si_roots.remove(0)))?;

        // state init's data's root cell contains zero-key
        // need to change it by real public key
        let new_data = insert_pubkey(
            state_init.data.clone().unwrap_or_default(),
            pub_key.as_bytes(),
        )?;
        state_init.set_data(new_data);

        let id = state_init.hash()?.into();

        Ok(Self{ state_init, id })
    }

    // Returns future contract's state_init struct
    pub fn state_init(self) -> StateInit {
        self.state_init
    }

    // Returns future contract's identifier
    pub fn account_id(&self) -> AccountId {
        self.id.clone()
    }

    ///Allows to change initial values for public contract variables
    pub fn update_data(&mut self, data_json: &str, abi_json: &str) -> SdkResult<()> {
        let contract = ton_abi::Contract::load(abi_json.as_bytes())
            .map_err(|err| SdkErrorKind::AbiError(err))?;

        let v: serde_json::Value = serde_json::from_str(&data_json)
            .map_err(|err| SdkErrorKind::SerdeJson(err))?;

        let params: Vec<_> = contract
            .data()
            .values()
            .map(|item| item.value.clone())
            .collect();

        let tokens = Tokenizer::tokenize_all(&params[..], &v)
            .map_err(|err| SdkErrorKind::AbiError(err))?;

        let mut new_data = self.state_init.data.clone().unwrap_or_default();
        for token in tokens {
            let builder = token.value.pack_into_chain()
                .map_err(|err| SdkErrorKind::AbiError(err))?;
            let key = contract
                .data()
                .get(&token.name)
                .ok_or(
                    SdkErrorKind::InvalidArg(format!("data item {} not found in contract ABI", token.name))
                )?.key;
            new_data = insert_data_item(new_data, key, builder)?;
        }
        self.state_init.set_data(new_data);
        self.id = self.state_init.hash()?.into();

        ok!()
    }
}


fn insert_pubkey(data: Arc<CellData>, pubkey: &[u8]) -> SdkResult<Arc<CellData>> {
    let pubkey_vec = pubkey.to_vec();
    let pubkey_len = pubkey_vec.len() * 8;
    let value = BuilderData::with_raw(pubkey_vec, pubkey_len)
            .unwrap_or(BuilderData::new()).into();
    insert_data_item(data, 0, value)
}

const DATA_MAP_KEYLEN: usize = 64;
fn insert_data_item(data: Arc<CellData>, key: u64, value: BuilderData) -> SdkResult<Arc<CellData>> {
    let mut map = HashmapE::with_data(
        DATA_MAP_KEYLEN, 
        data.into(),
    );
    map.set(
        //DATA_MAP_KEYLEN
        key.write_to_new_cell().unwrap().into(), 
        &value.into(), 
    ).map_err(|e| {
        SdkErrorKind::InternalError(
            format!("failed to update data item in data map: {}", e)
        )
    })?;
    let mut new_data = BuilderData::new();
    new_data.append_bit_one().unwrap()
        .checked_append_reference(map.data().unwrap()).unwrap();
    Ok(new_data.into())
}

/// Enum represents blockchain account address.
/// `Short` value contains only `AccountId` value and is used for addressing contracts in default
/// workchain. `Full` value is fully qualified account address and can be used for addressing
/// contracts in any workchain
#[derive(Clone)]
pub enum AccountAddress {
    Short(AccountId),
    Full(MsgAddressInt)
}

impl AccountAddress {
    /// Returns `AccountId` from the address
    pub fn get_account_id(&self) -> SdkResult<AccountId> {
        match self {
            AccountAddress::Short(account_id) => Ok(account_id.clone()),
            AccountAddress::Full(address) => {
                let vec = address.get_address();
                if vec.remaining_bits() == 256 {
                    Ok(vec)
                } else {
                    Err(SdkErrorKind::InvalidData("Address must be 32 bytes long".to_owned()).into())
                }
            }
        }
    }

    /// Returns full account address as `MsgAddressInt` struct
    pub fn get_msg_address(&self) -> SdkResult<MsgAddressInt> {
        match self {
            AccountAddress::Full(address) => Ok(address.clone()),
            AccountAddress::Short(id) => {
                let workchain = Contract::get_default_workchain()
                    .ok_or(SdkErrorKind::DefaultWorkchainNotSet)?;

                Ok(MsgAddressInt::with_standart(None, workchain as i8, id.clone())?)
            }
        }
    }

    /// Creates full account address from `AccountId` and workchain number
    pub fn with_account_id_and_workchain(workchain: i8, account_id: AccountId) -> SdkResult<Self> {
        Ok(AccountAddress::Full(MsgAddressInt::with_standart(None, workchain, account_id)?))
    }

    
    fn decode_std_base64(data: &str) -> SdkResult<Self> {
        // conversion from base64url
        let data = data.replace('_', "/").replace('-', "+");

        let vec = base64::decode(&data)?;

        // check CRC and address tag
        if State::<XMODEM>::calculate(&vec[..34]) != u16::from_be_bytes(<[u8; 2]>::try_from(&vec[34..36])?)
            || vec[0] & 0x3f != 0x11
        {
            bail!(SdkErrorKind::InvalidArg(data.to_owned()));
        };

        Ok(MsgAddressInt::with_standart(
                None,
                i8::from_be_bytes(<[u8; 1]>::try_from(&vec[1..2])?),
                vec[2..34].into())?
            .into())
    }

    fn decode_std_hex(data: &str) -> SdkResult<Self> {
        let vec: Vec<&str> = data.split(':').collect();

        if vec.len() != 2 {
            bail!(SdkErrorKind::InvalidArg(data.to_owned()));
        }

        Ok(MsgAddressInt::with_standart(
                None,
                i8::from_str_radix(vec[0], 10)?,
                hex::decode(vec[1])?.into())?
            .into())
    }
    
    /// Retrieves account address from `str` in Telegram lite-client format
    pub fn from_str(data: &str) -> SdkResult<Self> {
        if data.len() == 64 {
            Ok(AccountAddress::Short(hex::decode(data)?.into()))
        } else if data.len() == 48 {
            Self::decode_std_base64(data)
        } else {
            Self::decode_std_hex(data)
        }
    }

    fn get_std_address(&self) -> SdkResult<(i8, Vec<u8>)> {
        match self {
            AccountAddress::Full(address) => {
                match address {
                    MsgAddressInt::AddrStd(msg_address) => {
                        if msg_address.address.remaining_bits() != 256 {
                            bail!(SdkErrorKind::InvalidData("Address must be 32 bytes long".to_owned()));
                        }
                        Ok((msg_address.workchain_id, msg_address.address.get_bytestring(0)))
                    },
                    _ => bail!(SdkErrorKind::InvalidData("Non-std address".to_owned()))
                }
            },
            _ => bail!(SdkErrorKind::InvalidData("Non-std address".to_owned()))
        }
    }

    /// Returns base64 address representation
    pub fn as_base64(&self, bounceable: bool, test: bool, as_url: bool) -> SdkResult<String> {
        let (worckchain, mut address) = self.get_std_address()?;

        let mut tag = if bounceable { 0x11 } else { 0x51 };
        if test { tag |= 0x80 };
        let mut vec = vec![tag];
        vec.append(&mut worckchain.to_be_bytes().to_vec());
        vec.append(&mut address);
        
        vec.append(&mut State::<XMODEM>::calculate(&vec[..]).to_be_bytes().to_vec());

        let result = base64::encode(&vec);

        if as_url {
            Ok(result.replace('/', "_").replace('+', "-"))
        } else {
            Ok(result)
        }
    }
}

impl From<AccountId> for AccountAddress {
    fn from(data: AccountId) -> Self {
        AccountAddress::Short(data)
    }
}

impl From<MsgAddressInt> for AccountAddress {
    fn from(data: MsgAddressInt) -> Self {
        AccountAddress::Full(data)
    }
}


// The struct represents smart contract and allows
// to deploy and call it, to get some contract's properties.
// Don't forget - in TON blockchain Contract and Account are the same substances.
pub struct Contract {
    acc: Account,
}

#[allow(dead_code)]
#[cfg(feature = "node_interaction")]
impl Contract {

    // Asynchronously loads a Contract instance or None if contract with given id is not exists
    pub fn load(address: AccountAddress) -> SdkResult<Box<dyn Stream<Item = Option<Contract>, Error = SdkError>>> {
        let id = address.get_account_id()?;

        let map = queries_helper::load_record_fields(
            CONTRACTS_TABLE_NAME,
            &id.to_hex_string(),
            ACCOUNT_FIELDS)?
                .and_then(|val| {
                    if val == serde_json::Value::Null {
                        Ok(None)
                    } else {
                        println!("val {}", val);
                        let acc: Account = serde_json::from_value(val)
                            .map_err(|err| SdkErrorKind::InvalidData(format!("error parsing account: {}", err)))?;

                        Ok(Some(Contract { acc }))
                    }
            });

        Ok(Box::new(map))
    }

    // Asynchronously loads a Contract's json representation
    // or null if message with given id is not exists
    pub fn load_json(id: AccountId) -> SdkResult<Box<dyn Stream<Item = String, Error = SdkError>>> {

        let map = queries_helper::load_record_fields(CONTRACTS_TABLE_NAME, &id.to_hex_string(), ACCOUNT_FIELDS)?
            .map(|val| val.to_string());

        Ok(Box::new(map))
    }

    // Packs given inputs by abi and asynchronously calls contract.
    // Works with json representation of input and abi.
    // To get calling result - need to load message,
    // it's id and processing status is returned by this function
    pub fn call_json(id: AccountAddress, func: String, input: String, abi: String, key_pair: Option<&Keypair>)
        -> SdkResult<Box<dyn Stream<Item = ContractCallState, Error = SdkError>>> {

        // pack params into bag of cells via ABI
        let msg_body = ton_abi::encode_function_call(abi, func, input, key_pair)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let msg = Self::create_message(id.clone(), msg_body.into())?;

        // send message by Kafka
        let msg_id = Self::_send_message(msg)?;

        // subscribe on updates from DB and return updates stream
        Self::subscribe_updates(msg_id)
    }

    // Packs given image and input and asynchronously calls given contract's constructor method.
    // Works with json representation of input and abi.
    // To get calling result - need to load message,
    // it's id and processing status is returned by this function
    pub fn deploy_json(func: String, input: String, abi: String, image: ContractImage, key_pair: Option<&Keypair>)
        -> SdkResult<Box<dyn Stream<Item = ContractCallState, Error = SdkError>>> {

        let msg_body = ton_abi::encode_function_call(abi, func, input, key_pair)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let cell = msg_body.into();
        let msg = Self::create_deploy_message(Some(cell), image)?;

        let msg_id = Self::_send_message(msg)?;

        Self::subscribe_updates(msg_id)
    }

    // Packs given image asynchronously send deploy message into blockchain.
    // To get calling result - need to load message,
    // it's id and processing status is returned by this function
    pub fn deploy_no_constructor(image: ContractImage)
        -> SdkResult<Box<dyn Stream<Item = ContractCallState, Error = SdkError>>> {
        let msg = Self::create_deploy_message(None, image)?;

        let msg_id = Self::_send_message(msg)?;

        Self::subscribe_updates(msg_id)
    }

    // Asynchronously calls contract by sending given message.
    // To get calling result - need to load message,
    // it's id and processing status is returned by this function
    pub fn send_message(msg: TvmMessage)
        -> SdkResult<Box<dyn Stream<Item = ContractCallState, Error = SdkError>>> 
    {
        // send message by Kafka
        let msg_id = Self::_send_message(msg)?;
        // subscribe on updates from DB and return updates stream
        Self::subscribe_updates(msg_id)
    }

    fn _send_message(msg: TvmMessage) -> SdkResult<MessageId> {
        let (data, id) = Self::serialize_message(msg)?;

        requests_helper::send_message(&id.as_slice()[..], &data)?;
        //println!("msg is sent, id: {}", id.to_hex_string());
        Ok(id.clone())
    }

    pub fn send_serialized_message(id: MessageId, msg: &[u8]) -> SdkResult<()> {
        requests_helper::send_message(&id.as_slice()[..], msg)
    }

    pub fn subscribe_updates(message_id: MessageId) ->
        SdkResult<Box<dyn Stream<Item = ContractCallState, Error = SdkError>>> {

        let subscribe_stream = queries_helper::subscribe_record_updates(
            TRANSACTIONS_TABLE_NAME,
            &message_id.to_hex_string(), 
            CONTRACT_CALL_STATE_FIELDS)?
                .and_then(|value| {
                    Ok(serde_json::from_value::<ContractCallState>(value)?)
                });

        Ok(Box::new(subscribe_stream))
    }
}

pub struct MessageToSign {
    pub message: Vec<u8>,
    pub data_to_sign: Vec<u8>
}

impl Contract {
    /// Returns contract's identifier
    pub fn id(&self) -> SdkResult<AccountId> {
        Ok(self.acc.get_id()
            .ok_or(SdkErrorKind::InvalidData("No account ID".to_owned()))?
            .clone())
    }

    /// Returns contract's balance in NANO grams
    pub fn balance_grams(&self) -> SdkResult<Grams> {
        Ok(self.acc.get_balance()
            .ok_or(SdkErrorKind::InvalidData("No balance in account".to_owned()))?
            .grams
            .clone())
    }

    /// Returns contract's balance
    pub fn balance(&self) -> SdkResult<CurrencyCollection> {
        unimplemented!()
    }

    /// Returns blockchain's account struct
    /// Some node-specifed methods won't work. All TonStructVariant fields has Client variant.
    pub fn acc(&self) -> &Account {
         &self.acc
    }


    // ------- Decoding functions -------

    /// Creates `Contract` struct by data from database
    pub fn from_json(json: &str) -> SdkResult<Self> {
        let acc: Account = serde_json::from_str(json)?;

        Ok(Contract { acc })
    }

    /// Invokes local TVM instance with provided account data and inbound message.
    /// Returns outbound messages generated by contract function
    pub fn local_contract_call_by_data(state: StateInit, message: TvmMessage)
        -> SdkResult<Vec<TvmMessage>>
    {
        let code = state.code.ok_or(
            SdkError::from(SdkErrorKind::InvalidData("Account has no code".to_owned())))?;

        Ok(local_tvm::local_contract_call(code, state.data, &message)?)
    }

    /// Invokes local TVM instance with provided inbound message.
    /// Returns outbound messages generated by contract function
    pub fn local_call(&self, message: TvmMessage) -> SdkResult<Vec<TvmMessage>> {
        let state = self.acc.state().ok_or(
            SdkError::from(SdkErrorKind::InvalidData("Account has no state".to_owned())))?;

        if let AccountState::AccountActive(state) = state {
            Self::local_contract_call_by_data(state.clone(), message)
        } else {
            bail!(SdkErrorKind::InvalidData(format!("Account is not active. State: {}", state)))
        }
    }

    /// Invokes local TVM instance with provided inbound message.
    /// Returns outbound messages generated by contract function
    pub fn local_call_json(&self, func: String, input: String, abi: String, key_pair: Option<&Keypair>)
         -> SdkResult<Vec<TvmMessage>>
    {
        // pack params into bag of cells via ABI
        let msg_body = ton_abi::encode_function_call(abi, func, input, key_pair)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let address = self.id().unwrap_or(AccountId::from([0; 32])).into();

        let msg = Self::create_message(address, msg_body.into())?;

        self.local_call(msg)
    }

    /// Decodes output parameters returned by contract function call 
    pub fn decode_function_response_json(abi: String, function: String, response: SliceData) 
        -> SdkResult<String> {

        ton_abi::json_abi::decode_function_response(abi, function, response)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))
    }

    /// Decodes output parameters returned by contract function call from serialized message body
    pub fn decode_function_response_from_bytes_json(abi: String, function: String, response: &[u8])
        -> SdkResult<String> {

        let slice = Self::deserialize_tree_to_slice(response)?;

        Self::decode_function_response_json(abi, function, slice)
    }

    /// Decodes output parameters returned by contract function call 
    pub fn decode_unknown_function_response_json(abi: String, response: SliceData) 
        -> SdkResult<DecodedMessage> {

        ton_abi::json_abi::decode_unknown_function_response(abi, response)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))
    }

    /// Decodes output parameters returned by contract function call from serialized message body
    pub fn decode_unknown_function_response_from_bytes_json(abi: String, response: &[u8])
        -> SdkResult<DecodedMessage> {

        let slice = Self::deserialize_tree_to_slice(response)?;

        Self::decode_unknown_function_response_json(abi, slice)
    }

    /// Decodes output parameters returned by contract function call 
    pub fn decode_unknown_function_call_json(abi: String, response: SliceData) 
        -> SdkResult<DecodedMessage> {

        ton_abi::json_abi::decode_unknown_function_call(abi, response)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))
    }

    /// Decodes output parameters returned by contract function call from serialized message body
    pub fn decode_unknown_function_call_from_bytes_json(abi: String, response: &[u8])
        -> SdkResult<DecodedMessage> {

        let slice = Self::deserialize_tree_to_slice(response)?;

        Self::decode_unknown_function_call_json(abi, slice)
    }

    // ------- Call constructing functions -------

    // Packs given inputs by abi into Message struct.
    // Works with json representation of input and abi.
    // Returns message's bag of cells and identifier.
    pub fn construct_call_message_json(address: AccountAddress, func: String, input: String,
        abi: String, key_pair: Option<&Keypair>) -> SdkResult<(Vec<u8>, MessageId)> {

        // pack params into bag of cells via ABI
        let msg_body = ton_abi::encode_function_call(abi, func, input, key_pair)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let msg = Self::create_message(address, msg_body.into())?;

        Self::serialize_message(msg)
    }

    // Creates Message struct with provided body and account address
    // Returns message's bag of cells and identifier.
    pub fn construct_call_message_with_body(address: AccountAddress, body: &[u8]) -> SdkResult<(Vec<u8>, MessageId)> {
        let body_cell = Self::deserialize_tree_to_slice(body)?;

        let msg = Self::create_message(address, body_cell)?;

        Self::serialize_message(msg)
    }

    // Packs given inputs by abi into Message struct without sign and returns data to sign.
    // Sign should be then added with `add_sign_to_message` function
    // Works with json representation of input and abi.
    pub fn get_call_message_bytes_for_signing(address: AccountAddress, func: String, input: String, 
        abi: String) -> SdkResult<MessageToSign> {
        
        // pack params into bag of cells via ABI
        let (msg_body, data_to_sign) = ton_abi::prepare_function_call_for_sign(abi, func, input)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let msg = Self::create_message(address, msg_body.into())?;

        Self::serialize_message(msg).map(|(msg_data, _id)| {
                MessageToSign { message: msg_data, data_to_sign } 
            }
        )
    }

     // ------- Deploy constructing functions -------

    // Packs given image and input into Message struct.
    // Works with json representation of input and abi.
    // Returns message's bag of cells and identifier.
    pub fn construct_deploy_message_json(func: String, input: String, abi: String, image: ContractImage,
        key_pair: Option<&Keypair>) -> SdkResult<(Vec<u8>, MessageId)> {

        let msg_body = ton_abi::encode_function_call(abi, func, input, key_pair)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let cell = msg_body.into();
        let msg = Self::create_deploy_message(Some(cell), image)?;

        Self::serialize_message(msg)
    }

    // Packs given image and body into Message struct.
    // Returns message's bag of cells and identifier.
    pub fn construct_deploy_message_with_body(image: ContractImage, body: Option<&[u8]>) -> SdkResult<(Vec<u8>, MessageId)> {
        let body_cell = match body {
            None => None,
            Some(data) => Some(Self::deserialize_tree_to_slice(data)?)
        };

        let msg = Self::create_deploy_message(body_cell, image)?;
        
        Self::serialize_message(msg)
    }

    // Packs given image into Message struct.
    // Returns message's bag of cells and identifier.
    pub fn construct_deploy_message_no_constructor(image: ContractImage)
        -> SdkResult<(Vec<u8>, MessageId)>
    {
        let msg = Self::create_deploy_message(None, image)?;

        Self::serialize_message(msg)
    }
    
    // Packs given image and input into Message struct without sign and returns data to sign.
    // Sign should be then added with `add_sign_to_message` function
    // Works with json representation of input and abi.
    pub fn get_deploy_message_bytes_for_signing(func: String, input: String, abi: String,
        image: ContractImage) -> SdkResult<MessageToSign> {

        let (msg_body, data_to_sign) = ton_abi::prepare_function_call_for_sign(abi, func, input)
                .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        let cell = msg_body.into();
        let msg = Self::create_deploy_message(Some(cell), image)?;

        Self::serialize_message(msg).map(|(msg_data, _id)| {
                MessageToSign { message: msg_data, data_to_sign } 
            }
        )
    }


    // Add sign to message, returned by `get_deploy_message_bytes_for_signing` or 
    // `get_run_message_bytes_for_signing` function.
    // Returns serialized message and identifier.
    pub fn add_sign_to_message(signature: &[u8], public_key: &[u8], message: &[u8]) 
        -> SdkResult<(Vec<u8>, MessageId)> {
        
        let mut slice = Self::deserialize_tree_to_slice(message)?;

        let mut message: TvmMessage = TvmMessage::construct_from(&mut slice)?;

        let body = message.body()
            .ok_or(SdkError::from(SdkErrorKind::InvalidData("No message body".to_owned())))?;

        let signed_body = ton_abi::add_sign_to_function_call(signature, public_key, body)
            .map_err(|err| SdkError::from(SdkErrorKind::AbiError(err)))?;

        *message.body_mut() = Some(signed_body.into());
            

        Self::serialize_message(message)
    }

    fn create_message(address: AccountAddress, msg_body: SliceData) -> SdkResult<TvmMessage> {

        let mut msg_header = ExternalInboundMessageHeader::default();
        msg_header.dst = address.get_msg_address()?;
        
        let mut msg = TvmMessage::with_ext_in_header(msg_header);
        *msg.body_mut() = Some(msg_body);

        Ok(msg)
    }

    fn create_deploy_message(
        msg_body: Option<SliceData>,
        image: ContractImage
    ) -> SdkResult<TvmMessage> {
        let account_id = image.account_id();
        let state_init = image.state_init();
        let mut msg_header = ExternalInboundMessageHeader::default();
        msg_header.dst = AccountAddress::from(account_id).get_msg_address()?;
        let mut msg = TvmMessage::with_ext_in_header(msg_header);
        *msg.body_mut() = msg_body;
        *msg.state_init_mut() = Some(state_init);
        Ok(msg)
    }

    pub fn serialize_message(msg: TvmMessage) -> SdkResult<(Vec<u8>, MessageId)> {
        let cells = Arc::<CellData>::from(msg.write_to_new_cell()?);
        let id = cells.repr_hash();

        let mut data = Vec::new();
        let bag = BagOfCells::with_root(&cells);
        bag.write_to(&mut data, false)?;

        Ok((data, id.into()))
    }

    /// Deserializes tree of cells from byte array into `SliceData`
    fn deserialize_tree_to_slice(data: &[u8]) -> SdkResult<SliceData> {
        let mut response_cells = deserialize_cells_tree(&mut Cursor::new(data))?;

        if response_cells.len() != 1 {
            return Err(SdkError::from(SdkErrorKind::InvalidData("Deserialize message error".to_owned())));
        }

        Ok(response_cells.remove(0).into())
    }

    /// Deserializes TvmMessage from byte array
    pub fn deserialize_message(message: &[u8]) -> SdkResult<TvmMessage> {
        let mut root_cells = deserialize_cells_tree(&mut Cursor::new(message))?;

        if root_cells.len() != 1 { 
            return Err(SdkError::from(SdkErrorKind::InvalidData("Deserialize message error".to_owned())));
        }

        Ok(TvmMessage::construct_from(&mut root_cells.remove(0).into())?)
    }

    /// Sets new default workchain number which will be used in message destination address
    /// construction if client provides only account ID
    pub fn set_default_workchain(workchain: Option<i32>) {
        let mut default = DEFAULT_WORKCHAIN.lock().unwrap();
        *default = workchain;
    }

    /// Returns default workchain number which are used in message destination address
    /// construction if client provides only account ID
    pub fn get_default_workchain() -> Option<i32> {
        let default = DEFAULT_WORKCHAIN.lock().unwrap();

        default.clone()
    }
}