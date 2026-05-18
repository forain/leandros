//! aplay — PipeWire audio player for LeandrOS.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    write, STDOUT_FILENO, open, read, close, O_RDONLY,
    ipc_call, get_audio_port,
};
use ipc::Message;

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        let usage = b"Usage: aplay <file.wav|file.mid|test>\n";
        write(STDOUT_FILENO, usage.as_ptr(), usage.len());
        return 1;
    }

    let filename = core::slice::from_raw_parts(*argv.add(1), 256);
    let len = filename.iter().position(|&b| b == 0).unwrap_or(256);
    let filename_str = core::str::from_utf8_unchecked(&filename[..len]);

    // ── Get Audio Port from Kernel (via Auxv) ───────────────────────────────
    let pw_port = get_audio_port();

    if pw_port == u32::MAX {
        write_str("Error: Audio server port not found in auxv. Is PipeWire running?\n");
        return 1;
    }

    write_str("aplay: connected to audio server on port ");
    write_u32(pw_port);
    write_str("\n");

    if filename_str == "test" {
        play_test_tone(pw_port);
    } else if filename_str.ends_with(".wav") {
        play_wav(filename_str, pw_port);
    } else if filename_str.ends_with(".mid") {
        play_mid(filename_str, pw_port);
    } else {
        write_str("Unknown extension. Supported: .wav, .mid, test\n");
        return 1;
    }

    0
}

unsafe fn play_test_tone(port: u32) {
    write_str("aplay: generating 5s diagnostic tone...\n");
    set_audio_params(44100, 2, port);
    
    let mut phase = 0.0f32;
    let mut pcm = [0i16; 220];
    for _ in 0..1000 {
        for i in (0..220).step_by(2) {
            phase += 440.0 / 44100.0;
            if phase > 1.0 { phase -= 1.0; }
            let val = if phase < 0.5 { 15000 } else { -15000 }; 
            pcm[i] = val; pcm[i+1] = val;
        }
        send_pcm(core::slice::from_raw_parts(pcm.as_ptr() as *const u8, 440), port);
    }
}

unsafe fn play_wav(path: &str, port: u32) {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { write_str("aplay: failed to open file\n"); return; }
    let mut header = [0u8; 44];
    if read(fd, header.as_mut_ptr(), 44) < 44 { close(fd); return; }
    let channels = u16::from_le_bytes([header[22], header[23]]) as u8;
    let freq = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    set_audio_params(freq, channels, port);
    let mut buf = [0u8; 438];
    loop {
        let n = read(fd, buf.as_mut_ptr(), 438);
        if n <= 0 { break; }
        send_pcm(&buf[..n as usize], port);
    }
    close(fd);
}

unsafe fn set_audio_params(freq: u32, channels: u8, port: u32) {
    let mut msg = Message::empty();
    msg.tag = 0x100;
    msg.data[0..4].copy_from_slice(&freq.to_le_bytes());
    msg.data[4] = channels;
    ipc_call(port, &mut msg);
}

unsafe fn send_pcm(data: &[u8], port: u32) {
    let mut msg = Message::empty();
    msg.tag = 0x200;
    let len = data.len() as u16;
    msg.data[0] = (len & 0xFF) as u8;
    msg.data[1] = ((len >> 8) & 0xFF) as u8;
    msg.data[2..2 + data.len()].copy_from_slice(data);
    ipc_call(port, &mut msg);
}

static mut MIDI_DATA: [u8; 65536] = [0u8; 65536];

unsafe fn play_mid(path: &str, port: u32) {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return; }
    let n = read(fd, MIDI_DATA.as_mut_ptr(), 65536);
    close(fd);
    if n < 14 { return; }
    set_audio_params(44100, 1, port);
    let mut synth = Synth::new();
    let mut ptr = 14;
    if &MIDI_DATA[ptr..ptr+4] == b"MTrk" {
        let track_len = u32::from_be_bytes([MIDI_DATA[ptr+4], MIDI_DATA[ptr+5], MIDI_DATA[ptr+6], MIDI_DATA[ptr+7]]) as usize;
        ptr += 8;
        let track_end = ptr + track_len;
        let mut tempo = 500_000u32;
        while ptr < track_end {
            let mut val = 0u32;
            loop {
                let b = MIDI_DATA[ptr]; ptr += 1;
                val = (val << 7) | (b & 0x7F) as u32;
                if b & 0x80 == 0 { break; }
            }
            synth.generate_and_send((val as u64 * tempo as u64 * 44100 / (128 * 1_000_000)) as usize, port);
            let status = MIDI_DATA[ptr]; ptr += 1;
            match status & 0xF0 {
                0x80 => { synth.note_off(MIDI_DATA[ptr]); ptr += 2; }
                0x90 => { let note = MIDI_DATA[ptr]; if MIDI_DATA[ptr+1] == 0 { synth.note_off(note); } else { synth.note_on(note); } ptr += 2; }
                0xFF => { let t = MIDI_DATA[ptr]; ptr += 1;
                    let mut l = 0u32; loop { let b = MIDI_DATA[ptr]; ptr += 1; l = (l << 7) | (b & 0x7F) as u32; if b & 0x80 == 0 { break; } }
                    if t == 0x51 { tempo = ((MIDI_DATA[ptr] as u32) << 16) | ((MIDI_DATA[ptr+1] as u32) << 8) | (MIDI_DATA[ptr+2] as u32); }
                    ptr += l as usize;
                }
                _ => { if status < 0x80 { ptr -= 1; } else if status < 0xC0 || status >= 0xE0 { ptr += 2; } else { ptr += 1; } }
            }
        }
    }
}

struct Synth { active_notes: [Option<f32>; 16], phases: [f32; 16] }
impl Synth {
    fn new() -> Self { Self { active_notes: [None; 16], phases: [0.0; 16] } }
    fn note_on(&mut self, note: u8) {
        let f = get_note_freq(note);
        for i in 0..16 { if self.active_notes[i].is_none() { self.active_notes[i] = Some(f); self.phases[i] = 0.0; break; } }
    }
    fn note_off(&mut self, note: u8) {
        let f = get_note_freq(note);
        for i in 0..16 { if let Some(an) = self.active_notes[i] { let diff = if an > f { an - f } else { f - an }; if diff < 1.0 { self.active_notes[i] = None; } } }
    }
    unsafe fn generate_and_send(&mut self, mut samples: usize, port: u32) {
        let mut pcm = [0i16; 220];
        while samples > 0 {
            let n = if samples > 110 { 110 } else { samples };
            for i in 0..n {
                let mut s = 0f32; let mut count = 0;
                for j in 0..16 { if let Some(freq) = self.active_notes[j] {
                    self.phases[j] += freq / 44100.0; if self.phases[j] > 1.0 { self.phases[j] -= 1.0; }
                    s += if self.phases[j] < 0.5 { 12000.0 } else { -12000.0 }; count += 1;
                }}
                pcm[i*2] = if count > 0 { (s / count as f32) as i16 } else { 0 };
                pcm[i*2+1] = pcm[i*2];
            }
            send_pcm(core::slice::from_raw_parts(pcm.as_ptr() as *const u8, n * 4), port);
            samples -= n;
        }
    }
}

fn get_note_freq(note: u8) -> f32 {
    let mut f = 13.75f32;
    let note_in_octave = note % 12;
    let octave = (note / 12) as i32 - 1;
    let semi = [1.0, 1.05946, 1.12246, 1.18921, 1.25992, 1.33484, 1.41421, 1.49831, 1.5874, 1.68179, 1.7818, 1.88775];
    f *= semi[note_in_octave as usize];
    if octave > 0 { for _ in 0..octave { f *= 2.0; } }
    else if octave < 0 { for _ in 0..(-octave) { f /= 2.0; } }
    f
}

unsafe fn write_str(s: &str) { write(STDOUT_FILENO, s.as_ptr(), s.len()); }
unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 { write(STDOUT_FILENO, b"0".as_ptr(), 1); return; }
    let mut i = 10usize;
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}
