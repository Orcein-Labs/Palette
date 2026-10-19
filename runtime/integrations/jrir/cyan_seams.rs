// Cyan / Palette Rust <-> Julia Seam Kernels for JRIR (`runtime/integrations/jrir/`).
//
// 1. `compute_civil_dos`: C-ABI wrapper around the civil calendar (`unix_to_civil`)
//    and MS-DOS timestamp (`dos_datetime`) logic in `runtime/host/src/receipt_archive.rs:337-365`.
// 2. `crc32_ieee_slice`: C-ABI wrapper around the IEEE 802.3 CRC32 (`0xEDB88320`)
//    loop in `runtime/host/src/receipt_archive.rs:288-319`.
// 3. `strip_ansi_slice`: Exact zero-allocation byte-slice automaton matching
//    `src/digest.jl:9` (`const ANSI_ESCAPE = r"\e\[[0-9;?]*[A-Za-z]"`).
//
// Regenerate textual LLVM IR with:
//   rustc --crate-type=lib --emit=llvm-ir -C opt-level=3 -C target-cpu=native -C panic=abort \
//     runtime/integrations/jrir/cyan_seams.rs -o /tmp/cyan_seams.ll

#[repr(C)]
pub struct CivilDateTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    pub dos_time: u16,
    pub dos_date: u16,
}

#[no_mangle]
pub extern "C" fn compute_civil_dos(secs: i64, out: &mut CivilDateTime) -> u32 {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;

    let (dt, dd) = if year < 1980 {
        (0u16, (1u16 << 5) | 1u16)
    } else {
        let yc = year.min(2107) as u16;
        let t = ((hour as u16) << 11) | ((minute as u16) << 5) | ((second as u16) >> 1);
        let d = ((yc - 1980) << 9) | ((month as u16) << 5) | (day as u16);
        (t, d)
    };

    out.year = year;
    out.month = month;
    out.day = day;
    out.hour = hour;
    out.minute = minute;
    out.second = second;
    out.dos_time = dt;
    out.dos_date = dd;
    ((dd as u32) << 16) | (dt as u32)
}

#[no_mangle]
pub unsafe extern "C" fn crc32_ieee_slice(ptr: *const u8, len: usize, seed: u32) -> u32 {
    let mut crc = !seed;
    let mut i = 0usize;
    while i < len {
        crc ^= *ptr.add(i) as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320u32 & mask);
            bit += 1;
        }
        i += 1;
    }
    !crc
}

#[no_mangle]
pub unsafe extern "C" fn strip_ansi_slice(src: *const u8, len: usize, dst: *mut u8) -> usize {
    let mut r = 0usize;
    let mut w = 0usize;
    while r < len {
        let b = *src.add(r);
        if b == 0x1b && r + 1 < len && *src.add(r + 1) == b'[' {
            let mut k = r + 2;
            while k < len {
                let c = *src.add(k);
                if (c >= b'0' && c <= b'9') || c == b';' || c == b'?' {
                    k += 1;
                } else {
                    break;
                }
            }
            if k < len {
                let term = *src.add(k);
                if (term >= b'A' && term <= b'Z') || (term >= b'a' && term <= b'z') {
                    r = k + 1;
                    continue;
                }
            }
        }
        *dst.add(w) = b;
        w += 1;
        r += 1;
    }
    w
}
