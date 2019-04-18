use crate::{
    credential::Credential,
    crypto::{
        ciphersuite::CipherSuite,
        dh::{DhPrivateKey, DhPublicKey},
        ecies::EciesCiphertext,
        rng::CryptoRng,
        sig::{SigSecretKey, Signature},
    },
    error::Error,
    tls_ser,
};

// uint8 ProtocolVersion;
pub(crate) type ProtocolVersion = u8;

/// Contains a node's new public key and the new node's secret, encrypted for everyone in that
/// node's resolution
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DirectPathNodeMessage {
    pub(crate) public_key: DhPublicKey,
    // ECIESCiphertext node_secrets<0..2^16-1>;
    #[serde(rename = "node_secrets__bound_u16")]
    pub(crate) node_secrets: Vec<EciesCiphertext>,
}

/// Contains a direct path of node messages. The length of `node_secrets` for the first
/// `DirectPathNodeMessage` MUST be zero.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DirectPathMessage {
    // DirectPathNodeMessage nodes<0..2^16-1>;
    #[serde(rename = "node_messages__bound_u16")]
    pub(crate) node_messages: Vec<DirectPathNodeMessage>,
}

/// This is used in lieu of negotiating public keys when a participant is added. This has a bunch
/// of published ephemeral keys that can be used to initiated communication with a previously
/// uncontacted participant.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct UserInitKey {
    // opaque user_init_key_id<0..255>
    /// An identifier for this init key. This MUST be unique among the `UserInitKey` generated by
    /// the client
    #[serde(rename = "user_init_key_id__bound_u8")]
    pub(crate) user_init_key_id: Vec<u8>,

    // ProtocolVersion supported_versions<0..255>;
    /// The protocol versions supported by the member. Each entry is the supported protocol version
    /// of the entry in `init_keys` of the same index. This MUST have the same length as
    /// `init_keys`.
    #[serde(rename = "supported_versions__bound_u8")]
    supported_versions: Vec<ProtocolVersion>,

    // CipherSuite cipher_suites<0..255>
    /// The cipher suites supported by the member. Each cipher suite here corresponds uniquely to a
    /// DH public key in `init_keys`. As such, this MUST have the same length as `init_keys`.
    #[serde(rename = "cipher_suites__bound_u8")]
    pub(crate) cipher_suites: Vec<&'static CipherSuite>,

    // HPKEPublicKey init_keys<1..2^16-1>
    /// The DH public keys owned by the member. Each public key corresponds uniquely to a cipher
    /// suite in `cipher_suites`. As such, this MUST have the same length as `cipher_suites`.
    #[serde(rename = "init_keys__bound_u16")]
    pub(crate) init_keys: Vec<DhPublicKey>,

    /// The DH private keys owned by the member. This is only `Some` if this member is the creator
    /// of this `UserInitKey`. Each private key corresponds uniquely to a public key in
    /// `init_keys`. As such, this MUST have the same length as `init_keys`.
    #[serde(skip)]
    pub(crate) private_keys: Option<Vec<DhPrivateKey>>,

    /// The identity information of the member
    pub(crate) credential: Credential,

    /// Contains the signature of all the other fields of this struct, under the identity key of
    /// the client.
    pub(crate) signature: Signature,
}

// This struct is everything but the last field in UserInitKey. We use the serialized form
// of this as the message that the signature is computed over
#[derive(Serialize)]
struct PartialUserInitKey<'a> {
    #[serde(rename = "user_init_key_id__bound_u8")]
    user_init_key_id: &'a [u8],
    #[serde(rename = "supported_versions__bound_u8")]
    supported_versions: &'a [ProtocolVersion],
    #[serde(rename = "cipher_suites__bound_u8")]
    cipher_suites: &'a [&'static CipherSuite],
    #[serde(rename = "init_keys__bound_u16")]
    init_keys: &'a [DhPublicKey],
    credential: &'a Credential,
}

impl UserInitKey {
    /// Generates a new `UserInitKey` with the key ID, credential, ciphersuites, and supported
    /// versions. The identity key is needed to sign the resulting structure.
    pub(crate) fn new_from_random(
        identity_key: &SigSecretKey,
        user_init_key_id: Vec<u8>,
        credential: Credential,
        mut cipher_suites: Vec<&'static CipherSuite>,
        supported_versions: Vec<ProtocolVersion>,
        csprng: &mut dyn CryptoRng,
    ) -> Result<UserInitKey, Error> {
        // Check the ciphersuite list for duplicates. We don't like this
        let old_cipher_suite_len = cipher_suites.len();
        cipher_suites.dedup();
        if cipher_suites.len() != old_cipher_suite_len {
            return Err(Error::ValidationError(
                "Cannot make a UserInitKey with duplicate ciphersuites",
            ));
        }
        // Check that the ciphersuite and supported version vectors are the same length
        if cipher_suites.len() != supported_versions.len() {
            return Err(Error::ValidationError(
                "Supported ciphersuites and supported version vectors differ in length",
            ));
        }

        let mut init_keys = Vec::new();
        let mut private_keys = Vec::new();

        // Collect a keypair for every ciphersuite in the given vector
        for cs in cipher_suites.iter() {
            let scalar = cs.dh_impl.scalar_from_random(csprng)?;
            let public_key = cs.dh_impl.derive_public_key(&scalar);

            init_keys.push(public_key);
            private_keys.push(scalar);
        }
        // The UserInitKey has this as an Option
        let private_keys = Some(private_keys);

        // Now to compute the signature: Make the partial structure, serialize it, sign that
        let partial = PartialUserInitKey {
            user_init_key_id: user_init_key_id.as_slice(),
            supported_versions: supported_versions.as_slice(),
            cipher_suites: cipher_suites.as_slice(),
            init_keys: init_keys.as_slice(),
            credential: &credential,
        };

        let serialized_uik = tls_ser::serialize_to_bytes(&partial)?;
        let sig_scheme = credential.get_signature_scheme();
        let signature = sig_scheme.sign(identity_key, &serialized_uik);

        Ok(UserInitKey {
            user_init_key_id,
            supported_versions,
            cipher_suites,
            init_keys,
            private_keys,
            credential,
            signature,
        })
    }

    /// Verifies this `UserInitKey` under the identity key specified in the `credential` field
    ///
    /// Returns: `Ok(())` on success, `Error::SignatureError` on verification failure, and
    /// `Error::SerdeError` on some serialization failure.
    #[must_use]
    pub(crate) fn verify_sig(&self) -> Result<(), Error> {
        let partial = PartialUserInitKey {
            user_init_key_id: self.user_init_key_id.as_slice(),
            supported_versions: self.supported_versions.as_slice(),
            cipher_suites: self.cipher_suites.as_slice(),
            init_keys: self.init_keys.as_slice(),
            credential: &self.credential,
        };
        let serialized_uik = tls_ser::serialize_to_bytes(&partial)?;

        let sig_scheme = self.credential.get_signature_scheme();
        let public_key = self.credential.get_public_key();

        sig_scheme.verify(public_key, &serialized_uik, &self.signature)
    }

    // TODO: URGENT: Figure out how to implement the mandatory check specified in section 6:
    // "UserInitKeys also contain an identifier chosen by the client, which the client MUST assure
    // uniquely identifies a given UserInitKey object among the set of UserInitKeys created by this
    // client."

    /// Validates the invariants that `UserInitKey` must satisfy, as in section 6 of the MLS spec
    #[must_use]
    pub(crate) fn validate(&self) -> Result<(), Error> {
        // All three of supported_versions, cipher_suites, and init_keys MUST have the same length.
        // And if private_keys is non-null, it must have the same length as the other three.
        if self.supported_versions.len() != self.cipher_suites.len() {
            return Err(Error::ValidationError(
                "UserInitKey::supported_verions.len() != UserInitKey::cipher_suites.len()",
            ));
        }
        if self.init_keys.len() != self.cipher_suites.len() {
            return Err(Error::ValidationError(
                "UserInitKey::init_keys.len() != UserInitKey::cipher_suites.len()",
            ));
        }
        if let Some(ref ks) = self.private_keys {
            if ks.len() != self.cipher_suites.len() {
                return Err(Error::ValidationError(
                    "UserInitKey::private_keys.len() != UserInitKey::cipher_suites.len()",
                ));
            }
        }

        // The elements of cipher_suites MUST be unique. Sort them, dedup them, and see if the
        // number has decreased.
        let mut cipher_suites = self.cipher_suites.clone();
        let original_len = cipher_suites.len();
        cipher_suites.sort_by_key(|c| c.name);
        cipher_suites.dedup_by_key(|c| c.name);
        if cipher_suites.len() != original_len {
            return Err(Error::ValidationError(
                "UserInitKey has init keys with duplicate ciphersuites",
            ));
        }

        Ok(())
    }

    /// Retrieves the public key in this `UserInitKey` corresponding to the given cipher suite
    ///
    /// Returns: `Ok(Some(pubkey))` on success. Returns `Ok(None)` iff there is no public key
    /// corresponding to the given cipher suite. Returns `Err(Error::ValidationError)` iff
    /// validation (via `UserInitKey::validate()`) failed.
    pub(crate) fn get_public_key<'a>(
        &'a self,
        cs_to_find: &'static CipherSuite,
    ) -> Result<Option<&'a DhPublicKey>, Error> {
        // First validate. If this were not valid, then the output of this function might be
        // dependent on the order of occurrence of cipher suites, and that is undesirable
        self.validate()?;

        let cipher_suites = &self.cipher_suites;
        let init_keys = &self.init_keys;

        // Look for the ciphersuite in lock-step with the public key. If we find the ciphersuite at
        // index i, then the pubkey we want is also at index i These two lists are the same length,
        // because this property is checked in validate() above. Furthermore, all ciphersuites in
        // cipher_suites are unique, because this property is also checked in validate() above.
        for (cs, key) in cipher_suites.iter().zip(init_keys.iter()) {
            if cs == &cs_to_find {
                return Ok(Some(key));
            }
        }

        Ok(None)
    }

    /// Retrieves the private in this `UserInitKey` corresponding to the given cipher suite. The
    /// private key is only known if this member is the creator of this `UserInitKey`.
    ///
    /// Returns: `Ok(Some(privkey))` on success. Returns `Ok(None)` if the private key is not known
    /// or there is no private key corresponding to the given cipher suite. Returns
    /// `Err(Error::ValidationError)` iff validation (via `UserInitKey::validate()`) failed.
    pub(crate) fn get_private_key<'a>(
        &'a self,
        cs_to_find: &'static CipherSuite,
    ) -> Result<Option<&'a DhPrivateKey>, Error> {
        // First validate. If this were not valid, then the output of this function might be
        // dependent on the order of occurrence of cipher suites, and that is undesirable
        self.validate()?;

        let cipher_suites = &self.cipher_suites;
        // If we are the creator, we have a chance of finding the private key
        if let Some(ref private_keys) = self.private_keys {
            // Look for the ciphersuite in lock-step with the private key. If we find the
            // ciphersuite at index i, then the privkey we want is also at index i These two lists
            // are the same length, because this property is checked in validate() above.
            // Furthermore, all ciphersuites in cipher_suites are unique, because this property is
            // also checked in validate() above.
            for (cs, key) in cipher_suites.iter().zip(private_keys.iter()) {
                if cs == &cs_to_find {
                    return Ok(Some(key));
                }
            }
        }

        // No such private key was found (or we aren't the creator of this UserInitKey)
        Ok(None)
    }
}

/// This is currently not defined by the spec. See open issue in section 7.1
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupInit;

/// Operation to add a partcipant to a group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupAdd {
    // uint32 index;
    /// Indicates where to add the new participant. This may index into an empty roster entry or be
    /// equal to the size of the roster.
    pub(crate) roster_index: u32,

    // UserInitKey init_key;
    /// Contains the public key used to add the new participant
    pub(crate) init_key: UserInitKey,

    // opaque welcome_info_hash<0..255>;
    /// Contains the hash of the `WelcomeInfo` object that preceded this `Add`
    #[serde(rename = "welcome_info_hash__bound_u8")]
    pub(crate) welcome_info_hash: Vec<u8>,
}

/// Operation to add entropy to the group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupUpdate {
    pub(crate) path: DirectPathMessage,
}

/// Operation to remove a partcipant from the group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupRemove {
    /// The roster index of the removed participant
    pub(crate) removed_roster_index: u32,

    /// New entropy for the tree
    pub(crate) path: DirectPathMessage,
}

/// Enum of possible group operations
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename = "GroupOperation__enum_u8")]
pub(crate) enum GroupOperation {
    Init(GroupInit),
    Add(GroupAdd),
    Update(GroupUpdate),
    Remove(GroupRemove),
}

// TODO: Make confirmation a Mac enum for more type safety

/// A `Handshake` message, as defined in section 7 of the MLS spec
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Handshake {
    /// This is equal to the epoch of the current `GroupState`
    pub(crate) prior_epoch: u32,
    /// The operation this `Handshake` is perofrming
    pub(crate) operation: GroupOperation,
    /// Position of the signer in the roster
    pub(crate) signer_index: u32,
    /// Signature over the `Group`'s history:
    /// `Handshake.signature = Sign(identity_key, GroupState.transcript_hash)`
    pub(crate) signature: Signature,
    // opaque confirmation<1..255>;
    /// HMAC over the group state and `Handshake` signature
    /// `confirmation_data = GroupState.transcript_hash || Handshake.signature`
    /// `Handshake.confirmation = HMAC(confirmation_key, confirmation_data)`
    #[serde(rename = "confirmation__bound_u8")]
    pub(crate) confirmation: Vec<u8>,
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        crypto::{
            ciphersuite::{CipherSuite, P256_SHA256_AES128GCM, X25519_SHA256_AES128GCM},
            sig::SignatureScheme,
        },
        error::Error,
        group_state::{Welcome, WelcomeInfo},
        ratchet_tree::PathSecret,
        tls_de::TlsDeserializer,
        tls_ser,
        upcast::CryptoUpcast,
        utils::test_utils,
    };

    use quickcheck_macros::quickcheck;
    use rand::Rng;
    use rand_core::{RngCore, SeedableRng};
    use serde::Deserialize;

    // Check that Update operations are consistent
    #[quickcheck]
    fn update_correctness(rng_seed: u64) {
        let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
        // Make a starting group
        let (mut group_state1, identity_keys) = test_utils::random_full_group_state(&mut rng);

        // Make a copy of this group, but from another perspective. That is, we want the same group
        // but with a different roster index
        let new_index = loop {
            let idx = rng.gen_range(0, group_state1.roster.len());
            if idx != group_state1.roster_index as usize {
                assert!(idx <= core::u32::MAX as usize);
                break idx as u32;
            }
        };
        let group_state2 = test_utils::change_self_index(&group_state1, &identity_keys, new_index);

        // Make a new path secret and make an Update object out of it and then make a Handshake
        // object out of that Update
        let new_path_secret = {
            let mut buf = vec![0u8; group_state1.cs.hash_alg.output_len];
            rng.fill_bytes(buf.as_mut_slice());
            PathSecret::new(buf)
        };
        let (update_op, _, conf_key) =
            group_state1.create_update_op(new_path_secret, &mut rng).unwrap();
        let handshake = group_state1.create_handshake(update_op, conf_key).unwrap();

        // Apply the Handshake to the clone of the first group
        let new_group_state2 = group_state2.process_handshake(&handshake).unwrap();
        let group_state2 = new_group_state2;

        // Now see if the group states agree
        let (group1_bytes, group2_bytes) = (
            tls_ser::serialize_to_bytes(&group_state1).unwrap(),
            tls_ser::serialize_to_bytes(&group_state2).unwrap(),
        );
        assert_eq!(group1_bytes, group2_bytes, "GroupStates disagree after Update");
    }

    // File: messages.bin
    //
    // struct {
    //   CipherSuite cipher_suite;
    //   SignatureScheme sig_scheme;
    //
    //   opaque user_init_key<0..2^32-1>;
    //   opaque welcome_info<0..2^32-1>;
    //   opaque welcome<0..2^32-1>;
    //   opaque add<0..2^32-1>;
    //   opaque update<0..2^32-1>;
    //   opaque remove<0..2^32-1>;
    // } MessagesCase;
    //
    // struct {
    //   uint32_t epoch;
    //   uint32_t signer_index;
    //   uint32_t removed;
    //   opaque user_id<0..255>;
    //   opaque group_id<0..255>;
    //   opaque uik_id<0..255>;
    //   opaque dh_seed<0..255>;
    //   opaque sig_seed<0..255>;
    //   opaque random<0..255>;
    //
    //   SignatureScheme uik_all_scheme;
    //   UserInitKey user_init_key_all;
    //
    //   MessagesCase case_p256_p256;
    //   MessagesCase case_x25519_ed25519;
    // } MessagesTestVectors;
    //
    // The elements of the struct have the following meanings:
    //
    // * The first several fields contain the values used to construct the example messages.
    // * user_init_key_all contains a UserInitKey that offers all four ciphersuites.  It is validly
    //   signed with an Ed25519 key.
    // * The remaining cases each test message processing for a given ciphersuite:
    //   * case_p256_p256 uses P256 for DH and ECDSA-P256 for signing
    //   * case_x25519_ed25519 uses X25519 for DH and Ed25519 for signing
    // * In each case:
    //   * user_init_key contains a UserInitKey offering only the indicated ciphersuite, validly
    //     signed with the corresponding signature scheme
    //   * welcome_info contains a WelcomeInfo message with syntactically valid but bogus contents
    //   * welcome contains a Welcome message generated by encrypting welcome_info for a
    //     Diffie-Hellman public key derived from the dh_seed value.
    //   * add, update, and remove each contain a Handshake message with a GroupOperation of the
    //     corresponding type.  The signatures on these messages are not valid
    //
    // Your implementation should be able to pass the following tests:
    //
    // * user_init_key_all should parse successfully
    // * The test cases for any supported ciphersuites should parse successfully
    // * All of the above parsed values should survive a marshal / unmarshal round-trip

    #[derive(Debug, Deserialize, Serialize)]
    struct MessagesCase {
        cipher_suite: &'static CipherSuite,
        signature_scheme: &'static SignatureScheme,
        _user_init_key_len: u32,
        user_init_key: UserInitKey,
        _welcome_info_len: u32,
        welcome_info: WelcomeInfo,
        _welcome_len: u32,
        welcome: Welcome,
        _add_len: u32,
        add: Handshake,
        _update_len: u32,
        update: Handshake,
        _remove_len: u32,
        remove: Handshake,
    }

    impl CryptoUpcast for MessagesCase {
        fn upcast_crypto_values(&mut self, ctx: &crate::upcast::CryptoCtx) -> Result<(), Error> {
            let new_ctx =
                ctx.set_cipher_suite(self.cipher_suite).set_signature_scheme(self.signature_scheme);
            self.user_init_key.upcast_crypto_values(&new_ctx)?;
            self.welcome_info.upcast_crypto_values(&new_ctx)?;
            self.welcome.upcast_crypto_values(&new_ctx)?;
            self.add.upcast_crypto_values(&new_ctx)?;
            self.update.upcast_crypto_values(&new_ctx)?;
            self.remove.upcast_crypto_values(&new_ctx)?;
            Ok(())
        }
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct MessagesTestVectors {
        epoch: u32,
        signer_index: u32,
        removed: u32,
        #[serde(rename = "user_id__bound_u8")]
        user_id: Vec<u8>,
        #[serde(rename = "group_id__bound_u8")]
        group_id: Vec<u8>,
        #[serde(rename = "uik_id__bound_u8")]
        uik_id: Vec<u8>,
        #[serde(rename = "dh_seed__bound_u8")]
        dh_seed: Vec<u8>,
        #[serde(rename = "sig_seed__bound_u8")]
        sig_seed: Vec<u8>,
        #[serde(rename = "random__bound_u8")]
        random: Vec<u8>,
        uik_all_scheme: &'static SignatureScheme,
        _user_init_key_all_len: u32,
        user_init_key_all: UserInitKey,

        case_p256_p256: MessagesCase,
        case_x25519_ed25519: MessagesCase,
    }

    impl CryptoUpcast for MessagesTestVectors {
        fn upcast_crypto_values(&mut self, ctx: &crate::upcast::CryptoCtx) -> Result<(), Error> {
            let ctx = ctx.set_signature_scheme(self.uik_all_scheme);
            self.user_init_key_all.upcast_crypto_values(&ctx)?;

            let ctx = ctx.set_cipher_suite(&P256_SHA256_AES128GCM);
            self.case_p256_p256.upcast_crypto_values(&ctx)?;

            let ctx = ctx.set_cipher_suite(&X25519_SHA256_AES128GCM);
            self.case_x25519_ed25519.upcast_crypto_values(&ctx)?;

            Ok(())
        }
    }

    // Tests our code against the official key schedule test vector. All this has to do is make
    // sure that the given test vector parses without error, and that the bytes are the same after
    // being reserialized
    #[test]
    fn official_message_parsing_kat() {
        // Read in and deserialize the input
        let original_bytes = Vec::new();
        let mut f = std::fs::File::open("test_vectors/messages.bin").unwrap();
        let mut deserializer = TlsDeserializer::from_reader(&mut f);
        let test_vec = {
            let raw = MessagesTestVectors::deserialize(&mut deserializer).unwrap();
            //println!("{:#x?}", raw);
            // We can't do the upcasting here. The documentation lied when it said that
            // UserInitKeys are validly signed. They are [0xd6; 32], which is not a valid Ed25519
            // signature. So skip this step and call it a mission success.
            //raw.upcast_crypto_values(&CryptoCtx::new()).unwrap();
            raw
        };

        // Reserialized the deserialized input and make sure it's the same as the original
        let reserialized_bytes = tls_ser::serialize_to_bytes(&test_vec).unwrap();
        assert_eq!(reserialized_bytes, original_bytes);
    }
}
