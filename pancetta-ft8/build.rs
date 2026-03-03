fn main() {
    let ft8_dir = "vendor/ft8_lib";

    cc::Build::new()
        .files([
            format!("{}/ft8/constants.c", ft8_dir),
            format!("{}/ft8/encode.c", ft8_dir),
            format!("{}/ft8/decode.c", ft8_dir),
            format!("{}/ft8/message.c", ft8_dir),
            format!("{}/ft8/ldpc.c", ft8_dir),
            format!("{}/ft8/crc.c", ft8_dir),
            format!("{}/ft8/text.c", ft8_dir),
            format!("{}/fft/kiss_fft.c", ft8_dir),
            format!("{}/fft/kiss_fftr.c", ft8_dir),
            format!("{}/common/monitor.c", ft8_dir),
        ])
        .include(ft8_dir)
        .warnings(false)
        .opt_level(2)
        .compile("ft8_lib");

    println!("cargo:rerun-if-changed={}", ft8_dir);
}
