# FT8 Reference WAV Files

Real off-air FT8 recordings for decoder sanity checking. All files are 16-bit
mono PCM at 12000 Hz (~15 seconds each, ~351 KB).

## Sources

### jtdx/ (2 files)
- Source: https://sourceforge.net/projects/jtdx/files/samples/16bit_audio/FT8/
- `000000_000001.wav` - Synthetic test signal
- `190227_155815.wav` - Off-air recording, 2019-02-27

### wsjt/ (3 files)
- Source: https://sourceforge.net/projects/wsjt/files/samples/FT8/
- `210703_133430.wav` - Off-air recording, 2021-07-03
- `181201_180245.wav` - Off-air recording, 2018-12-01
- `170709_135615.wav` - Off-air recording, 2017-07-09

### basicft8/ (4 files)
- Source: https://github.com/rtmrtmrtmrtm/basicft8/tree/master/samples
- `170923_08200[0-4]5.wav` - Consecutive 15-second windows, 2017-09-23

## Usage

These files are used by decoder integration tests to verify that the decoder
can decode real FT8 signals. A correct FT8 decoder should find multiple
decoded messages in each file.

To get expected decode results for comparison, run these through WSJT-X or
ft8_lib's decoder.
