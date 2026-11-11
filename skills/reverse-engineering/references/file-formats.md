# File-format analysis

Work from samples. One file tells you little; several files that differ in known ways tell you most of the format.

## 1. Collect and control samples

- Gather as many samples as you can, of different sizes and ages.
- Best of all, produce samples yourself with the program that writes the format: an empty document, then one change at a time (one record, a longer name, a second record). Each pair isolates the bytes that encode one property.
- Keep a table of samples: name, size, what is known about its content.

## 2. First look

```sh
xxd sample | head -40
xxd sample | tail -20
strings -n 4 sample | head -50
```

Then a quick survey with a short script (Python through `shell` is enough):

- byte histogram and entropy per block: text (4 to 5 bits per byte), structured binary (5 to 7), compressed or encrypted (near 8);
- positions of long runs of `00` or `FF` (padding, alignment);
- repeated byte sequences at regular distances (records, chunk headers).

## 3. Recognise the family

| Shape | What you see | Examples |
|---|---|---|
| Header plus fixed records | A count near the start; file size = header + count × record size | Many game tables, simple databases |
| Chunked (tag, length, payload) | Four-character tags; lengths that jump to the next tag | RIFF, PNG, IFF |
| Type-length-value | Small tag bytes, length bytes, nested | ASN.1 DER, protobuf, many network formats |
| Offset table | A block of ascending 32-bit values pointing into the file | Archives, resource packs |
| Container of a known format | A magic number at an offset after an outer header | Zlib or Zstandard streams, embedded PNG, SQLite |
| Serialised object graph | Class names as strings, back-references | Java, .NET and Python serialisation |
| Text in disguise | JSON, XML, INI or Lua behind a thin header, XOR or compression | Configuration and save files |

Test compressed regions directly: try zlib, raw deflate, gzip, LZ4, Zstandard and LZMA at each candidate offset. A stream that decompresses cleanly is proof.

## 4. Find the fields

For each candidate field, state a hypothesis and test it across all samples.

- **Magic and version**: constant bytes at offset 0; a small integer after them that changes between program versions.
- **Sizes and counts**: values that equal the file size, the size minus the header, or the number of records. Check both byte orders.
- **Offsets**: values that are smaller than the file size, ascending, and land on plausible structure starts.
- **Strings**: length-prefixed (one, two or four bytes, or a variable-length integer) or terminated; fixed-width padded fields.
- **Timestamps**: 32-bit values near the current Unix time; 64-bit Windows FILETIME; days since an epoch.
- **Floats**: bytes that decode to round numbers as IEEE 754 (`00 00 80 3F` is 1.0).
- **Identifiers**: 16-byte fields with the version nibble of a UUID.
- **Checksums**: four or eight bytes that change completely when anything else changes. Try CRC-32 (several polynomials), Adler-32, and truncated common hashes over the obvious ranges: whole file with the field zeroed, payload only, header only.
- **Alignment**: structures padded to 4, 8 or 16 bytes; offsets that are all multiples of one value.

Differential analysis is the strongest tool: change one property in the writing program, save, and compare. A short script that prints differing ranges between two files does the work.

## 5. Confirm with the code

When the program that reads the format is available, use it to settle doubts: find the reader by the magic number, the file extension string or the error messages for a malformed file, and read the parsing function. Field order in the parser is field order in the file; error checks give the valid ranges.

## 6. Write a parser

The format is understood when a parser reads every sample without leftover bytes and without assumptions that fail.

- Write it early and extend it as fields are understood. Print unknown regions as hex with their offsets so nothing hides.
- Assert everything you believe: constant fields, size relations, checksum values. A failed assertion on a new sample is new information.
- If the task includes writing files, round-trip: parse then serialise each sample and compare byte for byte.

## 7. Document

A field table per structure:

| Offset | Size | Type | Name | Meaning | Confidence |
|---|---|---|---|---|---|
| 0x00 | 4 | bytes | `magic` | `SAVE` | confirmed |
| 0x04 | 2 | u16 LE | `version` | 3 in all samples | confirmed |
| 0x06 | 2 | u16 LE | `flags` | bit 0 = compressed | likely |
| 0x08 | 4 | u32 LE | `record_count` | number of records that follow | confirmed |

State byte order, alignment, string encoding, the compression and checksum algorithms with their exact parameters, and what remains unknown. Keep the parser and the sample table with the document.
