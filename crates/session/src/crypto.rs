use anyhow::Result;
use sha2::{Digest, Sha256};
use snow::StatelessTransportState;

const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Registration verifier sent to the signaling server. It is not the session key.
pub fn pass_verifier(password: &str) -> [u8; 32] {
    tagged_hash(b"lowlat-reg-v1", password)
}

/// Noise pre-shared key. A leaked verifier does not reveal this value.
pub fn pass_psk(password: &str) -> [u8; 32] {
    tagged_hash(b"lowlat-psk-v1", password)
}

fn tagged_hash(domain: &[u8], password: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(password.as_bytes());
    hasher.finalize().into()
}

pub struct NoiseSide {
    state: snow::HandshakeState,
}

impl NoiseSide {
    pub fn initiator(password: &str) -> Result<Self> {
        Self::build(password, true)
    }

    pub fn responder(password: &str) -> Result<Self> {
        Self::build(password, false)
    }

    fn build(password: &str, initiator: bool) -> Result<Self> {
        let params: snow::params::NoiseParams = PATTERN.parse()?;
        let psk = pass_psk(password);
        let builder = snow::Builder::new(params).psk(0, &psk);
        let state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        Ok(Self { state })
    }

    pub fn write(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; 256];
        let n = self.state.write_message(&[], &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn read(&mut self, message: &[u8]) -> Result<()> {
        let mut buf = vec![0u8; message.len() + 64];
        self.state.read_message(message, &mut buf)?;
        Ok(())
    }

    pub fn finish(self) -> Result<StatelessTransportState> {
        Ok(self.state.into_stateless_transport_mode()?)
    }
}

pub fn seal(state: &mut StatelessTransportState, nonce: u64, plain: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; plain.len() + 32];
    let n = state.write_message(nonce, plain, &mut out)?;
    out.truncate(n);
    Ok(out)
}

pub fn open(state: &mut StatelessTransportState, nonce: u64, cipher: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; cipher.len()];
    let n = state.read_message(nonce, cipher, &mut out)?;
    out.truncate(n);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_and_psk_differ() {
        assert_ne!(pass_verifier("secret"), pass_psk("secret"));
    }

    #[test]
    fn wrong_password_fails_the_handshake() {
        let mut init = NoiseSide::initiator("secret").unwrap();
        let mut resp = NoiseSide::responder("nope").unwrap();
        let msg = init.write().unwrap();
        assert!(resp.read(&msg).is_err());
    }

    #[test]
    fn stateless_transport_survives_a_skipped_nonce() {
        let mut init = NoiseSide::initiator("secret").unwrap();
        let mut resp = NoiseSide::responder("secret").unwrap();
        let msg = init.write().unwrap();
        resp.read(&msg).unwrap();
        let reply = resp.write().unwrap();
        init.read(&reply).unwrap();
        let mut send = init.finish().unwrap();
        let mut recv = resp.finish().unwrap();
        let first = seal(&mut send, 1, b"one").unwrap();
        let third = seal(&mut send, 3, b"three").unwrap();
        assert_eq!(open(&mut recv, 3, &third).unwrap(), b"three");
        assert_eq!(open(&mut recv, 1, &first).unwrap(), b"one");
    }
}
