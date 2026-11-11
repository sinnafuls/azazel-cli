# Protocol analysis

Applies to network protocols and to any exchange between two parts of a system: sockets, pipes, serial lines, IPC messages. Analyse traffic of systems the user operates or is authorised to test.

## 1. Get captures you can reason about

- Ask the user for a capture (pcap, proxy log, serial log) or for permission and a setup to record one. Prefer captures of single, known actions: connect, log in, send one message, disconnect.
- Record what was done and when, so messages can be matched to actions.
- Capture the same action several times, and the same action with one input changed. Repetition separates constants from session values; variation shows where inputs land.
- If the transport is TLS, analysis needs either the client's key log (many clients honour `SSLKEYLOGFILE`), an intercepting proxy the client is configured to trust, or the plaintext taken from the application side. If none is available, analyse the client statically instead.

## 2. Transport and framing

Establish the layers from the outside in:

1. Transport: TCP, UDP, QUIC, WebSocket, HTTP requests, a pipe.
2. Security: TLS, DTLS, a custom handshake.
3. Framing: how message boundaries are marked on a byte stream.

Framing patterns:

| Pattern | Sign |
|---|---|
| Length prefix | A 1, 2 or 4-byte value at the start equal to the bytes that follow, sometimes including itself |
| Delimiter | Lines ending in `\n` or `\r\n`, or a zero byte |
| Fixed size | Every message the same length |
| Type then length | A tag byte or word, then a length, then the body |
| Self-describing | JSON, MessagePack, CBOR, protobuf, ASN.1 |

Reassemble the stream before looking for messages; a message split across packets or several messages in one packet are both normal.

## 3. Message structure

Line the messages up, one per line in hex, grouped by direction and by action. Then classify each field:

- **Constant** across all messages: magic, protocol version.
- **Constant per message type**: the type or opcode. Build the table of opcodes and the action that produces each.
- **Increasing**: sequence numbers, timestamps, counters.
- **Echoed**: a value sent by one side and repeated by the other is a request identifier or a challenge.
- **Session-constant**: the same within one connection and different across connections: session identifiers, keys, tokens.
- **Dependent on input**: appears when you change what was typed or sent; encoded how (plain, length-prefixed, compressed, encrypted).
- **High entropy and unpredictable**: ciphertext, random nonces, signatures, hashes. Note lengths: 16, 20, 32 and 64 bytes suggest specific algorithms.

Try the common encodings on bodies before assuming a custom one: protobuf (decode without a schema and see if the structure is sane), MessagePack, CBOR, JSON behind compression, base64 or hex layers.

## 4. The state machine

Write the conversation as states and transitions: which message opens the exchange, what each side must have received before sending each type, which messages are optional, what ends the session, how errors are reported, what keep-alives look like and how often they occur. A sequence diagram of one complete session belongs in the report.

## 5. Authentication, integrity and encryption

Describe what is there; do not attempt to break it beyond what the task authorises.

- Identify the handshake: who proves what to whom, which values are fresh per session.
- For a checksum or signature field, determine from the client code which bytes are covered and which algorithm and key are used. Captures alone rarely settle this; static analysis of the sending function does.
- Find the algorithm by constants and library calls in the client (see the decompiler workflow), and the key source by following the key argument backwards.
- Record replay behaviour only if testing against the live service is in scope.

## 6. Confirm from the implementation

The client and server code are the specification. In the binary, start from the socket send and receive calls, or from the serialisation library, and walk outward to the function that builds each message type. The dispatcher on the receiving side is usually a switch on the opcode: its cases give you the complete message list, including messages you never captured.

## 7. Prove it

Write a decoder that takes a capture and prints each message with named fields. It must decode every captured message with no unexplained bytes. If the task calls for it and the user has authorised talking to the service, write a minimal client that completes one exchange; that is the strongest proof.

## 8. Document

- Transport, port, security layer, framing rule.
- A table per message type: opcode, direction, fields (offset, size, type, name, meaning), when it is sent.
- The sequence diagram and state table.
- Algorithms with exact parameters for checksums, compression and cryptography, and where keys come from.
- The decoder, the captures used (or how to reproduce them), and open questions.
