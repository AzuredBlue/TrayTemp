//! Pure register decoding for CPU package temperature. Platform-neutral core.

/// Intel package temperature in degrees C.
///
/// `msr_1a2` is MSR_TEMPERATURE_TARGET, `msr_1b1` is IA32_PACKAGE_THERM_STATUS.
/// Returns `None` when the reading-valid bit (31) is clear, when TjMax is 0, or
/// when the digital readout exceeds TjMax (a negative temperature is garbage).
pub fn intel_package_temp(msr_1a2: u64, msr_1b1: u64) -> Option<f32> {
    let tj_max = ((msr_1a2 >> 16) & 0xFF) as u32;
    let readout = ((msr_1b1 >> 16) & 0x7F) as u32;
    if msr_1b1 & (1 << 31) == 0 || tj_max == 0 {
        return None;
    }
    tj_max.checked_sub(readout).map(|t| t as f32)
}

/// AMD Zen temperature in degrees C from SMN 0x0005_9800 (THM_TCON_CUR_TMP).
pub fn amd_tctl(raw: u32) -> f32 {
    let t = ((raw >> 21) & 0x7FF) as f32 / 8.0;
    if raw & 0x80000 != 0 {
        t - 49.0
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intel_basic() {
        assert_eq!(intel_package_temp(100 << 16, (1 << 31) | (45 << 16)), Some(55.0));
    }
    #[test]
    fn intel_invalid_bit() {
        assert_eq!(intel_package_temp(100 << 16, 45 << 16), None);
    }
    #[test]
    fn amd_plain() {
        assert_eq!(amd_tctl(440 << 21), 55.0);
    }
    #[test]
    fn amd_range_select() {
        assert_eq!(amd_tctl((832 << 21) | 0x80000), 55.0);
    }
}
