/// RC4 stream cipher as used by the AST2100 video engine.
pub struct Rc4 {
    state: [u8; 256],
    x: u8,
    y: u8,
}

impl Rc4 {
    /// The key is repeated cyclically over 256 bytes before scheduling.
    pub fn new(key: &[u8]) -> Self {
        assert!(!key.is_empty(), "RC4 key must not be empty");
        let mut state = [0_u8; 256];
        for (i, slot) in state.iter_mut().enumerate() {
            *slot = i as u8;
        }
        let mut j = 0_u8;
        for i in 0..256 {
            j = j.wrapping_add(state[i]).wrapping_add(key[i % key.len()]);
            state.swap(i, j as usize);
        }
        Self { state, x: 0, y: 0 }
    }

    pub fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            self.x = self.x.wrapping_add(1);
            let a = self.state[self.x as usize];
            self.y = self.y.wrapping_add(a);
            let b = self.state[self.y as usize];
            self.state[self.x as usize] = b;
            self.state[self.y as usize] = a;
            *byte ^= self.state[a.wrapping_add(b) as usize];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_rfc6229_vector() {
        // RFC 6229, 40-bit key 0x0102030405, keystream offset 0.
        let mut cipher = Rc4::new(&[1, 2, 3, 4, 5]);
        let mut data = [0_u8; 8];
        cipher.apply(&mut data);
        assert_eq!(data, [0xb2, 0x39, 0x63, 0x05, 0xf0, 0x3d, 0xc0, 0x27]);
    }
}
