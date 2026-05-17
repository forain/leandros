//! aplay — PipeWire audio player for LeandrOS.
//!
//! Supports .wav (PCM) and .mid (via simple square-wave synth).

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    write, STDOUT_FILENO, open, read, close, O_RDONLY,
    ipc_call,
};
use ipc::Message;

const PW_PORT: u32 = 3;

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        let usage = b"Usage: aplay <file.wav|file.mid>\n";
        write(STDOUT_FILENO, usage.as_ptr(), usage.len());
        return 1;
    }

    let msg = b"aplay: playing through PipeWire port ";
    write(STDOUT_FILENO, msg.as_ptr(), msg.len());
    write_u32(PW_PORT);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);

    let filename = core::slice::from_raw_parts(*argv.add(1), 256);
    let len = filename.iter().position(|&b| b == 0).unwrap_or(256);
    let filename_str = core::str::from_utf8_unchecked(&filename[..len]);

    if filename_str.ends_with(".wav") {
        play_wav(filename_str);
    } else if filename_str.ends_with(".mid") {
        play_mid(filename_str);
    } else {
        let err = b"Unknown file extension. Supported: .wav, .mid\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        return 1;
    }

    0
}

unsafe fn play_wav(path: &str) {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        let err = b"Could not open file\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        return;
    }

    let mut riff = [0u8; 12];
    if read(fd, riff.as_mut_ptr(), 12) < 12 || &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        let err = b"Not a valid WAVE file\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        close(fd);
        return;
    }

    let mut channels = 1u8;
    let mut sample_rate = 44100u32;
    let mut data_found = false;

    // Chunk parsing loop
    let mut chunk_hdr = [0u8; 8];
    while read(fd, chunk_hdr.as_mut_ptr(), 8) == 8 {
        let chunk_id = &chunk_hdr[0..4];
        let chunk_size = u32::from_le_bytes([chunk_hdr[4], chunk_hdr[5], chunk_hdr[6], chunk_hdr[7]]);

        if chunk_id == b"fmt " {
            let mut fmt = [0u8; 16];
            if chunk_size >= 16 && read(fd, fmt.as_mut_ptr(), 16) == 16 {
                channels = u16::from_le_bytes([fmt[2], fmt[3]]) as u8;
                sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                if chunk_size > 16 {
                    leandros_libc::lseek(fd, (chunk_size - 16) as i64, 1); // SEEK_CUR
                }
            } else {
                leandros_libc::lseek(fd, chunk_size as i64, 1);
            }
        } else if chunk_id == b"data" {
            data_found = true;
            set_audio_params(sample_rate, channels);
            
            let mut buf = [0u8; 438];
            let mut remaining = chunk_size;
            while remaining > 0 {
                let to_read = if remaining > 438 { 438 } else { remaining as usize };
                let n = read(fd, buf.as_mut_ptr(), to_read);
                if n <= 0 { break; }
                send_pcm(&buf[..n as usize]);
                remaining -= n as u32;
            }
            break; // Finished playing data
        } else {
            // Skip unknown chunk
            leandros_libc::lseek(fd, chunk_size as i64, 1);
        }
    }

    if !data_found {
        let err = b"No data chunk found in WAV file\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
    }

    close(fd);
}

unsafe fn set_audio_params(freq: u32, channels: u8) {
    let mut msg = Message::empty();
    msg.tag = 0x100; // SET_PARAMS
    msg.data[0..4].copy_from_slice(&freq.to_le_bytes());
    msg.data[4] = channels;
    ipc_call(PW_PORT, &mut msg);
}

unsafe fn send_pcm(data: &[u8]) {
    let mut msg = Message::empty();
    msg.tag = 0x200; // PCM_DATA
    let len = data.len() as u16;
    msg.data[0] = (len & 0xFF) as u8;
    msg.data[1] = ((len >> 8) & 0xFF) as u8;
    msg.data[2..2 + data.len()].copy_from_slice(data);
    ipc_call(PW_PORT, &mut msg);
}

static mut MIDI_DATA: [u8; 65536] = [0u8; 65536];

unsafe fn play_mid(path: &str) {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        let err = b"Could not open MIDI file\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        return;
    }

    let n = read(fd, MIDI_DATA.as_mut_ptr(), 65536);
    close(fd);

    if n < 14 {
        let err = b"MIDI file too small\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        return;
    }

    if &MIDI_DATA[0..4] != b"MThd" {
        let err = b"Not a MIDI file (missing MThd)\n";
        write(STDOUT_FILENO, err.as_ptr(), err.len());
        return;
    }

    let division = u16::from_be_bytes([MIDI_DATA[12], MIDI_DATA[13]]);
    
    set_audio_params(44100, 1);

    let mut synth = Synth::new();
    let mut ptr = 14;
    
    // Simplistic: only play the first track
    if &MIDI_DATA[ptr..ptr+4] == b"MTrk" {
        let track_len = u32::from_be_bytes([MIDI_DATA[ptr+4], MIDI_DATA[ptr+5], MIDI_DATA[ptr+6], MIDI_DATA[ptr+7]]) as usize;
        ptr += 8;
        let track_end = ptr + track_len;
        
        let mut tempo = 500_000u32; // Default 120 BPM
        
        while ptr < track_end {
            let delta_time = read_varlen(&MIDI_DATA, &mut ptr);
            
            let samples = (delta_time as u64 * tempo as u64 * 44100 / (division as u64 * 1_000_000)) as usize;
            synth.generate_and_send(samples);
            
            let status = MIDI_DATA[ptr];
            ptr += 1;
            
            match status & 0xF0 {
                0x80 => { // Note Off
                    let note = MIDI_DATA[ptr];
                    synth.note_off(note);
                    ptr += 2;
                }
                0x90 => { // Note On
                    let note = MIDI_DATA[ptr];
                    let velocity = MIDI_DATA[ptr+1];
                    if velocity == 0 { synth.note_off(note); }
                    else { synth.note_on(note); }
                    ptr += 2;
                }
                0xFF => { // Meta event
                    let type_ = MIDI_DATA[ptr];
                    ptr += 1;
                    let len = read_varlen(&MIDI_DATA, &mut ptr);
                    if type_ == 0x51 && len == 3 { // Set Tempo
                        tempo = ((MIDI_DATA[ptr] as u32) << 16) | ((MIDI_DATA[ptr+1] as u32) << 8) | (MIDI_DATA[ptr+2] as u32);
                    }
                    ptr += len as usize;
                }
                _ => {
                    if status < 0x80 { // Running status
                        ptr -= 1;
                    } else if status < 0xC0 || status >= 0xE0 {
                        ptr += 2;
                    } else {
                        ptr += 1;
                    }
                }
            }
        }
    }
}

fn read_varlen(data: &[u8], ptr: &mut usize) -> u32 {
    let mut val = 0u32;
    loop {
        let b = data[*ptr];
        *ptr += 1;
        val = (val << 7) | (b & 0x7F) as u32;
        if b & 0x80 == 0 { break; }
    }
    val
}

struct Synth {
    active_notes: [Option<f32>; 16],
    phases: [f32; 16],
}

impl Synth {
    fn new() -> Self {
        Self { active_notes: [None; 16], phases: [0.0; 16] }
    }

    fn note_on(&mut self, note: u8) {
        let freq = get_note_freq(note);
        for i in 0..16 {
            if self.active_notes[i].is_none() {
                self.active_notes[i] = Some(freq);
                self.phases[i] = 0.0;
                break;
            }
        }
    }

    fn note_off(&mut self, note: u8) {
        let freq = get_note_freq(note);
        for i in 0..16 {
            if let Some(f) = self.active_notes[i] {
                let diff = if f > freq { f - freq } else { freq - f };
                if diff < 0.1 {
                    self.active_notes[i] = None;
                }
            }
        }
    }

    unsafe fn generate_and_send(&mut self, mut samples: usize) {
        let mut pcm = [0i16; 219];
        while samples > 0 {
            let n = if samples > 219 { 219 } else { samples };
            for i in 0..n {
                let mut sample = 0f32;
                let mut count = 0;
                for j in 0..16 {
                    if let Some(freq) = self.active_notes[j] {
                        self.phases[j] += freq / 44100.0;
                        if self.phases[j] > 1.0 { self.phases[j] -= 1.0; }
                        sample += if self.phases[j] < 0.5 { 1.0 } else { -1.0 };
                        count += 1;
                    }
                }
                if count > 0 {
                    pcm[i] = (sample * 1000.0 / count as f32) as i16;
                } else {
                    pcm[i] = 0;
                }
            }
            let bytes = core::slice::from_raw_parts(pcm.as_ptr() as *const u8, n * 2);
            send_pcm(bytes);
            samples -= n;
        }
    }
}

fn get_note_freq(note: u8) -> f32 {
    let octave = (note / 12) as i32 - 1;
    let note_in_octave = note % 12;
    let base_freqs = [
        8.1758f32,  // C0
        8.6619f32,  // C#0
        9.1770f32,  // D0
        9.7227f32,  // D#0
        10.3009f32, // E0
        10.9134f32, // F0
        11.5623f32, // F#0
        12.2499f32, // G0
        12.9783f32, // G#0
        13.7500f32, // A0
        14.5676f32, // A#0
        15.4339f32, // B0
    ];
    
    let mut f = base_freqs[note_in_octave as usize];
    if octave > 0 {
        for _ in 0..octave { f *= 2.0; }
    } else if octave < 0 {
        for _ in 0..(-octave) { f /= 2.0; }
    }
    f
}

unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 { write(STDOUT_FILENO, b"0".as_ptr(), 1); return; }
    let mut i = 10usize;
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}
