# 00 — Horismos: definitions before anything else

Aristotle begins an inquiry by defining its subject (*horismos*). A definition names
the **genus**, the wider kind a thing belongs to, and the **differentia**, the
feature that separates it from everything else in that kind. "A human is an animal
(genus) that reasons (differentia)."

This file defines every term the project relies on, in that form. The rules:

1. **Order.** The Genus, Differentia and Note of a row use only primitives (§0) and
   terms defined in earlier rows. Nothing is defined in terms of itself, directly or
   through a chain. A note may point to a later section by number only, never by
   using a term that is defined there.
2. **Irreducible.** Terms are broken down until they reach a primitive.
3. **One sense per entry.** When a word has two meanings (for example "index"), each
   meaning gets its own entry and a qualifier in parentheses.
4. **Sources.** Protocol terms follow the BitTorrent Enhancement Proposals (BEPs) at
   bittorrent.org, read in full on 2026-09-16. Where a BEP's prose and its examples
   disagree, the note says which one is followed and why. Legal and historical claims
   cite their source and say how certain they are.

Later documents build on this one:
[01-first-principles](01-first-principles.md) derives requirements from these
definitions, [02-legacy-audit](02-legacy-audit.md) applies them to dhtcrawler2,
[03-design](03-design.md) is the resulting design, and
[04-operations](04-operations.md) tells an operator how to run it.

Contents: §0 primitives · §1 data and programs · §2 networking · §3 BitTorrent content ·
§4 the DHT · §5 fetching metadata · §6 search · §7 storage and processing · §8 the
words in the request · §9 security, law and governance · §10 building and operating.

---

## §0 Primitives

A chain of definitions has to stop somewhere. Aristotle calls the starting points of an
inquiry *archai*. For terms, the starting points are primitives: terms taken as
understood and not defined here. Each row says why it is not broken down further.

| Primitive | What we take it to mean | Why it is not defined further |
|---|---|---|
| **bit** | one base-2 digit, 0 or 1 | the smallest unit of information |
| **byte** | an ordered group of 8 bits (a value 0–255) | fixed by universal convention |
| **byte string** | a finite ordered sequence of bytes, possibly empty | only needs *byte* and *sequence* |
| **integer** | a whole number, positive, negative or zero | basic arithmetic |
| **instant / duration** | a point in time / an amount of time | basic to all experience |
| **computer** | a machine that stores byte strings and executes instructions | the physical ground of everything below |
| **program** | a set of instructions a computer can execute | only needs *computer* and *instruction* |
| **process** | one running execution of a program | only needs *program* |
| **message** | a byte string that one process sends to another | only needs *byte string* and *process* |
| **person** | a human being | everyday |
| **organisation** | a group of persons that acts as one (a company, a charity, a court, a government body) | everyday; these documents never analyse it |
| **law** | rules that a state makes and enforces | everyday; the documents cite particular laws, not the nature of law |
| **function** (mathematical) | a rule that assigns exactly one output to each input | basic mathematics |
| **everyday words** | *set, sequence, list, pair, triple, part, whole, kind, thing, property, relation, rule, name, value, number, record, field* (a named part of a record), *document* (a written work, such as each part of this series), *to map* (to assign one thing to another), *instruction, action, event, state, change, purpose, harm, measure, limit, rate, random, language, writing* in their ordinary senses | Definitions are built from these words. Defining them would need the words themselves. When one of them is used in a narrower technical sense, that sense gets its own entry below. |

---

## §1 Data and programs

### §1.1 General terms

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **data** | byte strings | held or exchanged because they represent something (numbers, words, records) | |
| **component** | part | a distinct part of a larger working whole that can be built, replaced or examined on its own | |
| **system** | set of components | the components work together as one whole toward one purpose | In 01–04, "the system" means the one those documents specify. |
| **artifact** | thing | made by persons for a purpose, so its form and its end come from its maker and not from its own nature | Aristotle, *Physics* II.1. Programs are artifacts. |
| **the project** | undertaking | the rewrite that documents 00–04 describe | "This project" in a note always means this one. |
| **procedure** | rule | a fixed sequence of steps for doing something | |
| **atomic (operation)** | property of an operation | it either completes entirely or has no effect at all | Not the sense of "atomic" in rule 2 above, which is why that rule is called *Irreducible*. |
| **CPU / memory / disk** | parts of a computer | the **CPU** executes instructions; **memory** holds byte strings only while the computer runs; a **disk** keeps them when the power is off | |
| **durable** | property of stored data | it survives the end of the process that wrote it and a restart of the computer | In practice this means it is on disk. |
| **resource** | thing a computer has in limited amount | CPU time, memory, disk space or bandwidth | |
| **bounded** | property of a use of a resource | it has an explicit, enforced upper limit | |
| **budget** | limit | an allowance of a resource, per period or in total, that use spends until it runs out | |
| **operator** | person or organisation | runs a system for others and answers for it | |
| **user** | person | uses the output of a system | |
| **asset** | thing of value | something an operator or user wants to protect | For example a computer, stored data, or a person's privacy. §9.1 lists the ones this project protects. |
| **adversary (attacker)** | person or organisation | intends to harm an asset | |
| **threat** | possible event | could harm an asset | |
| **attack** | action | an adversary's attempt to harm an asset | |
| **control (security)** | measure | reduces the likelihood or the impact of a threat | |
| **defect (bug)** | flaw in an artifact | makes it behave other than its maker intended | |
| **vulnerability** | defect | lets a threat happen | |
| **confidentiality / integrity / availability** | security properties | **confidentiality**: data is seen only by those allowed to see it; **integrity**: data and behaviour are changed only by those allowed to change them; **availability**: the system works when it is needed | |
| **denial of service (DoS)** | attack | exhausts a resource so that a system stops working for others | Any unbounded use of a resource can make it possible. |

### §1.2 Numbers, units and time

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **big-endian** | byte order | writes the most significant byte of a number first | |
| **KiB / MiB / GiB** | units of size | 1 KiB = 1 024 bytes; 1 MiB = 1 024 KiB; 1 GiB = 1 024 MiB | MB and GB (10⁶ and 10⁹ bytes) appear only in rough figures. The "16 MB" size limit mentioned in 02 is 16 MiB. |
| **Unix time** | integer | the number of seconds since 1970-01-01 00:00:00 UTC, not counting leap seconds | |
| **p95 (95th percentile)** | statistic of a set of measurements | the value that 95% of the measurements do not exceed | |

### §1.3 Characters and text

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **encoding** | rule | turns values of some kind into sequences of symbols (such as bytes) and back, without loss | |
| **character** | abstract unit of writing | the smallest such unit that has its own identity in a writing standard: a letter, digit, punctuation mark, ideograph, space, or an invisible mark that controls layout or devices (e.g. "a", "東", line feed) | |
| **coded character set** | catalogue of characters | gives each character a number | |
| **code point** | integer | the number that a coded character set gives to one character | |
| **writing system (script)** | set of characters | used together to write one or more languages (Latin, Cyrillic, Arabic…) | The other sense of "script" is in §2.3. |
| **Unicode** | coded character set | the single universal set, maintained by the Unicode Consortium, that aims to cover every writing system; its code points run from U+0000 to U+10FFFF | |
| **ASCII** | coded character set | the 7-bit US set of 128 characters, which are also Unicode's first 128 code points | |
| **text** | sequence of characters | read as written language (a name, a sentence, a search request) | |
| **whitespace** | set of characters | those shown as blank space or line breaks (space, tab, line feed…) | |
| **text encoding** | encoding | turns text into byte strings and back | |
| **UTF-8** | text encoding | the Unicode encoding that uses 1–4 bytes per code point and leaves ASCII text unchanged | Not every byte string is valid UTF-8. UTF-16 and UTF-32 are the other Unicode encodings. |
| **pre-Unicode text encoding** | text encoding | is not one of the Unicode encodings (UTF-8/16/32) and was designed for one language, region or vendor (GBK, GB18030, Big5, Shift_JIS, windows-1252…) | 01 R8 refers to these. GB18030 is a Chinese national standard that can also represent all of Unicode. |
| **hexadecimal (hex)** | encoding of byte strings as text | writes each byte as two characters from `0-9a-f` | A 20-byte value is 40 hex characters. This project writes lowercase and accepts both cases. |
| **base32** | encoding of byte strings as text | uses 32 symbols (`A-Z2-7`), each carrying 5 bits (RFC 4648) | A 20-byte value is 32 base32 characters. |
| **combining mark** | character | is drawn on or next to the character before it, such as an accent (U+0301) | |
| **confusable (homoglyph)** | character | looks like a different character (Cyrillic "а" and Latin "a") | |
| **normalization (Unicode)** | transformation of text | maps every code-point sequence to one representative of the class of sequences that Unicode declares equivalent to it. **NFC** applies only strict equivalence ("é" as one code point equals "e" plus a combining acute accent). **NFKC** also applies compatibility equivalence, which folds formatting variants such as full-width letters ("ｆｕｌｌ" → "full"), ligatures ("ﬁ" → "fi") and circled digits ("①" → "1"). | Neither form maps confusables from different writing systems to each other. |
| **confusable skeleton (UTS #39)** | transformation of text | maps each confusable to one chosen prototype character, so two strings that look alike get the same skeleton | Unicode Technical Standard #39. |
| **case folding** | transformation of text | maps upper- and lower-case forms to one form for comparison, using Unicode's full case-folding table ("ß" and "SS" both become "ss") | |
| **lowercasing** | transformation of text | replaces each upper-case letter with its lower-case form | Not the same as case folding: "STRASSE" becomes "strasse", but "straße" stays "straße", so the two still differ. |
| **accent folding (ASCII folding)** | transformation of text | removes combining marks and maps each Latin letter to its closest ASCII letter ("é" → "e") | |
| **control character** | character | Unicode category Cc: C0 (U+0000–U+001F), DEL (U+007F) and C1 (U+0080–U+009F); has no visible form and was designed to control devices | Can hide or corrupt displayed text. |
| **format character** | character | Unicode category Cf: has no visible form of its own but changes how neighbouring characters are displayed or joined | Includes the zero-width characters (U+200B–U+200D, U+2060, U+FEFF) and the soft hyphen (U+00AD). |
| **bidi control** | format character | changes the direction in which surrounding text is displayed (U+061C, U+200E–F, U+202A–E, U+2066–9) | Can make `gpj.exe` display as `exe.jpg`. |
| **default-ignorable code point** | code point | Unicode says that a program which cannot render it should show nothing for it | Includes the zero-width characters, the soft hyphen and the bidi controls. |
| **noncharacter** | code point | Unicode permanently reserves it never to be a character (U+FDD0–U+FDEF, and the last two code points of each 65 536-code-point plane) | |
| **leetspeak (leet)** | writing style | replaces letters with digits or symbols that look similar ("3" for "e", "0" for "o", "$" for "s") | Used to slip words past filters. |
| **markup language** | text format | adds marks (such as `<b>…</b>`) to text to give it structure | |

### §1.4 Data structures

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **data structure** | arrangement of data in memory or on disk | chosen so that particular operations (lookup, insertion, ordering) are efficient | |
| **key (lookup)** | value | identifies one entry in a collection, so the entry can be found by it | Other senses of "key" have their own entries, qualified by section (§1.5, §1.6, §3, §7). |
| **map (dictionary)** | data structure | holds key→value entries, at most one entry per key | |
| **LRU map** | map | has a size cap and, when full, evicts the least recently used entry | |
| **TTL (time to live)** | duration | after which an entry expires | |
| **tree** | data structure | its elements hang from one root, and every element except the root has exactly one parent | |
| **queue** | data structure | holds items waiting to be processed, taken out roughly in the order they were put in | |
| **ordered stream** | sequence | its items are produced over time and read in order | |

### §1.5 Programs

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **operating system** | program | manages a computer's resources and runs other programs as processes | |
| **Linux** | operating system | the open-source, Unix-like operating system this project runs on | |
| **file system** | part of an operating system | keeps named byte strings durably on disk, in a tree of named folders called **directories** | |
| **file** | named byte string | kept by a file system | |
| **path** | sequence of names | locates a file: each name (a **path component**) selects one directory level, and the last selects the file | `.` and `..` are special components meaning "this directory" and "the parent directory". |
| **programming language** | formal language | expresses instructions that can be turned into a program | |
| **source code** | text | written in a programming language; programs are built from it | |
| **machine code** | instructions | in the form a CPU executes directly | |
| **compiler** | program | translates source code into machine code | |
| **binary (executable)** | file | holds machine code that the operating system can run as a process | *Precompiled* means shipped as a binary instead of as source code. |
| **subroutine (function in code)** | named unit of code | can be called with inputs (arguments) and returns a result | "Function" in code names (`binary_to_term`) has this sense. |
| **recursion** | way of writing a subroutine | the subroutine calls itself, for example once per level of nesting | The opposite, **iterative** code, uses a loop and an explicit, bounded list instead. |
| **stack (call stack)** | region of memory | a running program uses it to track subroutine calls; it has a fixed size, so very deep recursion overflows it | |
| **library** | body of code | offers subroutines for programs to call; it does not run by itself | |
| **shared library** | library | kept in its own file and loaded into a process when the program starts, instead of being copied into the binary | |
| **C library (glibc)** | shared library | the standard runtime library that almost every Linux program uses; glibc is the GNU implementation | |
| **self-contained binary** | binary | the whole program is one executable file that needs no shared library except the C library | A *statically linked* binary would not need even that. This project's binary is self-contained but not statically linked (03 §14). |
| **DLL** | shared library | in the Windows file format | |
| **dependency** | library or tool | a program is built with it or calls it, and someone else wrote it | |
| **thread** | line of execution | one of possibly several inside a process, all sharing its memory | |
| **concurrent** | property of units of work | they make progress over the same period of time | |
| **task** | unit of concurrent work | lighter than a thread: an *asynchronous runtime* runs many tasks on a few threads and switches between them whenever one waits | |
| **memory-safe language** | programming language | rules out out-of-bounds access, use-after-free and data races by construction, so a bug cannot corrupt memory or run injected machine code | |
| **Rust** | memory-safe language | compiles to machine code without a garbage collector; only code marked `unsafe` may bypass its checks | |
| **Go** | memory-safe language | compiles to machine code and uses a garbage collector | |
| **crate** | library or program | Rust's unit of compilation and distribution | A *workspace* is a set of crates built together with one shared record of the exact dependencies used. This project's crates forbid `unsafe` code (`#![forbid(unsafe_code)]`). |
| **panic** | failure mode (Rust) | stops the current thread when an assumption in the code is violated | A panic that input can trigger is a denial of service. |
| **cargo** | program | Rust's standard tool for compiling crates and fetching them from crates.io | `cargo audit` checks dependencies against the RustSec advisory list. `cargo deny` also checks licences, sources and duplicates. `clippy` is Rust's linter. |
| **toolchain** | set of programs | the compiler, linker and project tool for a language | This project pins rustc and cargo 1.98.1 in `rust-toolchain.toml`. |
| **CLI (command-line interface)** | interface | text commands typed in a terminal; a **subcommand** is the word after the program name that selects the action | |
| **environment variable** | named string | set for a process by whatever starts it, and readable by the process | |
| **configuration** | set of values | changes a program's behaviour without changing its code | Each value is named by a **configuration key**. |
| **log** | record of events | written by a program as it runs, each line with an importance level (`trace` is the most detailed) | |
| **structured log** | log | each entry is a set of named fields, not free text | |
| **API (application programming interface)** | interface | the set of calls that one program offers to others | |

### §1.6 Hashing and cryptography

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **hash function** | function (mathematical) from byte strings to byte strings | its output has a fixed length | |
| **digest (hash)** | byte string | the output of a hash function for one input | |
| **non-cryptographic hash function** | hash function | built for speed and an even spread of outputs; someone who wants to can easily find two inputs with the same output | |
| **checksum** | non-cryptographic hash function | its output is stored or sent with data to detect accidental change | It offers **no** protection against deliberate change. |
| **CRC32C** | checksum | 32-bit output, computed with the Castagnoli polynomial | |
| **cryptographic hash function** | hash function | designed so that it is infeasible to find two inputs with the same output (**collision resistance**), an input with a given output (**preimage resistance**), or a second input with the same output as a given one (**second-preimage resistance**) | |
| **broken (cryptographic property)** | state of a property of a cryptographic function | an attack that defeats the property in practice has been shown | A function with a broken property is still a cryptographic hash function, because the definition is about its design. |
| **SHA-1** | cryptographic hash function | the FIPS 180-4 function with a 20-byte digest | Its collision resistance is broken (practical collisions in 2017 and 2020). Its preimage and second-preimage resistance still hold. |
| **SHA-256** | cryptographic hash function | the FIPS 180-4 function with a 32-byte digest | No practical attack on any of its three properties is known. |
| **multihash** | byte string | a digest preceded by a code for its hash function and by its length, each as a variable-length integer | `1220` means SHA-256 (0x12), 32 bytes (0x20). |
| **Merkle tree** | tree of digests | each leaf is the digest of one block of data and each inner element is the digest of its children, so one root digest covers all the data | |
| **hash table** | map | finds an entry by computing a non-cryptographic hash of its key | Lookups take constant expected time. |
| **Bloom filter** | probabilistic set | sets several bits chosen by hash functions for each member, so it can answer "definitely not present" or "probably present" in very little memory | |
| **secret** | byte string | known only to the parties meant to know it, so that a protection fails if anyone else learns it | |
| **cryptographic key** | byte string | a parameter of a cryptographic algorithm; a *private key* is a secret, and the matching *public key* may be shared | |
| **digital signature** | byte string | computed from data with a private key, so anyone with the matching public key can check that the key holder approved exactly that data | |

### §1.7 Structured data and parsing

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **serialization format** | encoding | encodes structured values (numbers, lists, maps) rather than only text | |
| **canonical (encoding)** | property of an encoded value | it obeys every ordering and minimality rule of its format, so each value has exactly one canonical encoding | |
| **JSON** | serialization format | text-based (RFC 8259): objects, arrays, strings, numbers, booleans and null | |
| **TOML** | serialization format | text-based and meant for configuration: `key = value` pairs grouped under `[section]` headers | This project's configuration is a TOML file, and environment variables named `DC3_<SECTION>__<KEY>` override it. |
| **YAML** | serialization format | text-based, with structure shown by indentation | |
| **XML** | markup language | general-purpose, for structured data | A **CDATA section** (`<![CDATA[…]]>`) carries raw text and ends at the first `]]>`, so text containing `]]>` must be split or escaped. |
| **compression** | encoding | shortens byte strings by removing redundancy | |
| **DEFLATE / gzip / zlib** | compression formats | DEFLATE is the algorithm; gzip and zlib wrap it with headers and a checksum | To *gunzip* is to decompress gzip. |
| **lz4** | compression format | very fast, with a modest compression ratio | |
| **parser (decoder)** | component | turns a byte string into structured values, or reports that it cannot | |
| **bounded parser** | parser | enforces explicit limits on input size, nesting depth, string length and element count, and returns an error (never a crash) on any input | It is the first code that outside input reaches, so it is the main defence. |
| **prefix decoding** | mode of a parser | reads one value from the start of a longer byte string and reports where the value ended | |
| **raw span** | byte string | the exact slice of the input that a parsed value came from | Hashing a raw span avoids any error a re-encoding could introduce. |

---

## §2 Networking

### §2.1 Addresses and transport

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **network** | set of computers, or of processes on them | its members can exchange messages with each other | |
| **host** | computer | attached to a network | |
| **protocol** | set of rules | fixes the format and order of messages between processes | |
| **client / server (roles)** | roles of a process | the **client** sends a request; the **server** receives it and answers | One process can play both roles. "Server" also names a host that runs servers. |
| **packet (datagram)** | message | sent across a network as one unit, with no guarantee that it arrives | |
| **Internet Protocol (IP)** | protocol | carries packets between hosts across interconnected networks; each packet carries a numeric source and destination address | |
| **IP address** | number | of 32 bits (**IPv4**) or 128 bits (**IPv6**) that IP uses to name one network attachment of a host | |
| **address family** | kind of IP address | IPv4 or IPv6 | |
| **the Internet** | network | the public, global network of networks that uses IP | |
| **prefix (IP)** | set of IP addresses | they share their first *n* bits, written `/n` (IPv4 `/24`, IPv6 `/64`) | |
| **router** | host | forwards packets between networks | |
| **CIDR (Classless Inter-Domain Routing)** | way of dividing IP addresses | allocates addresses, and lets routers forward them, as prefixes of any length; its notation writes a prefix as `address/n` (`192.0.2.0/24`, `2001:db8::/32`) | |
| **ISP (Internet service provider)** | organisation | connects its customers' networks to the Internet | |
| **NAT (network address translation)** | address-translation service of a router | rewrites packet addresses so that many hosts on an inner network share one outer IP address | Hides the inner address and blocks unsolicited inbound packets. |
| **CGNAT (carrier-grade NAT)** | NAT | run by an ISP, so that many customers share one IPv4 address | It uses the range 100.64.0.0/10 on the inner side. |
| **customer allocation** | prefix | the IP addresses an ISP gives one customer: usually a single IPv4 address (a /32), often shared through CGNAT, and an IPv6 prefix of /64 to /48 | This is why 03 §12 keys its per-client request limits on the IPv4 /32, and on the IPv6 /64, /56 and /48 **at the same time**: the size of a client's IPv6 allocation is not known. |
| **unspecified address** | IP address | `0.0.0.0` (IPv4) or `::` (IPv6); names no host | |
| **loopback address (localhost)** | IP address | in `127.0.0.0/8` or equal to `::1`; reaches only the same host | |
| **private address** | IP address | reserved for inner networks and not forwarded on the Internet: `10.0.0.0/8`, `172.16.0.0/12` and `192.168.0.0/16` (IPv4), and the unique-local range `fc00::/7` (IPv6, **ULA**) | |
| **link-local address** | IP address | valid only on one physical network link: `169.254.0.0/16` or `fe80::/10` | |
| **multicast address** | IP address | names a group of hosts rather than one: `224.0.0.0/4` or `ff00::/8` | |
| **IPv4-mapped IPv6 address** | IPv6 address | `::ffff:a.b.c.d`, which represents an IPv4 address inside IPv6 | The older, deprecated *IPv4-compatible* form is `::a.b.c.d`. |
| **6to4 / Teredo / NAT64 address** | IPv6 address | embeds an IPv4 address for a transition mechanism (`2002::/16`, `2001::/32`, `64:ff9b::/96`) | |
| **IANA** | organisation | allocates IP address space and protocol numbers worldwide | |
| **special-purpose address** | IP address | in a range that IANA's special-purpose registries reserve for something other than ordinary public addressing | Includes all the kinds above, plus the documentation, benchmarking, broadcast and reserved ranges. |
| **bogon** | IP address | should never appear as a public source or destination, because it is special-purpose or not yet allocated | |
| **internal network** | network | cannot be reached from the Internet | |
| **ASN (autonomous system number)** | number | identifies one network that the Internet treats as a single routing unit | Checks run "from different ASNs" run from different networks. |
| **transport protocol** | protocol | carries data between processes on different hosts, on top of IP | |
| **UDP** | transport protocol | sends each packet on its own: nothing is set up beforehand, and delivery and order are not guaranteed | |
| **connection** | association between two processes | set up before data flows and kept until either side closes it | |
| **TCP** | transport protocol | sets up a connection that carries an ordered, reliable byte stream in each direction | |
| **port** | integer 0–65535 | selects which process receives traffic, among those using the same transport protocol on one IP address | UDP port 6881 and TCP port 6881 are different ports. Port 0 is not a valid destination. |
| **endpoint (socket address)** | pair | an IP address plus a port | |
| **socket** | operating-system object | a process's handle for sending and receiving with one transport protocol. *Binding* attaches it to a local endpoint (the unspecified address means "every local address"). A *listener* is a bound TCP socket that accepts connections. | An IPv6 socket with `IPV6_V6ONLY` set handles IPv6 traffic only. |
| **application protocol** | protocol | defines one application's messages and is carried by a transport protocol | |
| **overlay network** | network | its members are processes that address each other by their own identifiers and exchange messages over an underlying network such as the Internet | |
| **distributed hash table (DHT)** | overlay network | stores key→value entries across its members, each member holding the entries whose keys are closest to its own identifier under a fixed distance function, and lets any member find an entry in a small number of steps | Unlike a hash table (§1.6), a lookup takes about log *n* steps across the network, not constant time. |
| **Tor** | overlay network | hides who talks to whom by routing traffic through several relays | |

### §2.2 Names

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **domain name** | name | a sequence of dot-separated labels (e.g. `btdig.com`) registered in one global naming hierarchy | |
| **DNS (Domain Name System)** | naming system | a hierarchy of servers that answers requests mapping domain names to typed records: **A** (IPv4 address), **AAAA** (IPv6 address), **NS** (the name servers for a zone), **DS** (a digest that links a zone's signing key to its parent), **TXT** (free text) | A *zone* is the part of the hierarchy that one set of name servers answers for. |
| **reverse DNS** | DNS lookup | maps an IP address back to a domain name | |
| **glue record** | DNS record | gives a name server's IP address in the parent zone, so that the server can be found | |
| **DNSSEC** | addition to DNS | signs DNS records, so that a resolver can detect forged answers | |
| **registry (domain)** | organisation | runs one top-level domain (such as `.com`) and its master list of registrations | |
| **registrar** | organisation | sells and administers domain registrations on behalf of registries | Whoever controls the registrar account controls the domain. |
| **registration record (WHOIS / RDAP)** | public record | a domain's registrar, dates, status flags and name servers, published through the WHOIS or RDAP protocols | |
| **registrar lock** | status flag | `clientTransferProhibited` / `clientUpdateProhibited`: blocks transfer or change of the domain until the account holder removes it | |
| **registry lock** | status flag | set by the registry (`serverTransferProhibited` and similar); lifting it needs confirmation outside the registrar account | |
| **transferred / re-registered domain** | domain name | *transferred*: moved to another registrar or holder while registered; *re-registered*: allowed to lapse and then registered by someone new | |
| **parked domain** | domain name | registered but serving only placeholder or advertising pages | |
| **lookalike domain** | domain name | registered to be mistaken for another one, for example through a swapped letter or a confusable | |

### §2.3 Encryption in transit and the web

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **certificate** | signed statement | binds a public key to a domain name; issued by a *certificate authority* (CA) that clients trust | |
| **certificate transparency (CT)** | set of public append-only logs | records every certificate that CAs issue, so a domain holder can spot certificates they did not request | |
| **CAA record** | DNS record | lists which certificate authorities may issue certificates for a domain | |
| **TLS** | protocol | encrypts and authenticates a TCP connection using certificates | |
| **URL / URI** | text identifier | a URI names a resource; a URL also says how to reach it: scheme, host, port, path, and parameters after `?` | |
| **percent-encoding** | encoding of bytes in URLs | writes each byte that is unsafe in a URL as `%` followed by two hex digits | |
| **HTTP** | application protocol | request/response protocol over TCP: a request names a method and a URL; requests and responses carry **headers** (named fields) and a body; each response has a numeric **status** (200 OK, 404 Not Found, 410 Gone, 429 Too Many Requests…) | **HTTPS** is HTTP inside TLS. |
| **route (HTTP)** | pair of an HTTP method and a URL path pattern | one entry point of an HTTP server (e.g. `GET /search`) | A *route template* is the pattern itself (`/t/{key}`). 01 and 04 call routes "endpoints"; that is a different sense from *endpoint (socket address)* in §2.1. |
| **World Wide Web (the web)** | system of hypertext | its pages are linked by URLs and fetched over HTTP | |
| **HTML** | markup language | describes the pages of the web | |
| **web page** | page of the web | written in HTML, fetched by URL and shown to a person | |
| **browser** | HTTP client program | fetches, displays and runs web pages for a person | |
| **JavaScript** | programming language | runs inside browsers and can read and change a page and send requests | |
| **script (web)** | program | JavaScript source code that a web page loads or contains | |
| **directory listing** | web page | generated by a server to show the files in a directory | |
| **redirect** | behaviour of an HTTP response | makes the browser load another URL: a 3xx status with a `Location` header, a `Refresh` header, `<meta http-equiv="refresh">` (a **meta refresh**), or a script that sets `location` | |
| **origin** | triple | the scheme, host and port of a URL | Browsers isolate pages of different origins. *Same-origin* means the triple is equal. |
| **website** | set of web pages and files | served over HTTP under one domain name for people to use through a browser | |
| **visitor** | person | uses a website through a browser | |
| **onion service** | website | reachable only through Tor, at a `.onion` name | Deferred (03 §15). |
| **User-Agent** | HTTP request header | names the client software | |
| **Fetch Metadata headers (`Sec-Fetch-*`)** | HTTP request headers | set by the browser, not by page script, to say what kind of request it is (`Sec-Fetch-Dest`, `Sec-Fetch-Mode`) and where it came from (`Sec-Fetch-Site`) | |
| **reverse proxy** | server | receives visitors' HTTP requests and forwards them to the application's server, usually handling TLS itself, and names the client's IP address in the `X-Forwarded-For` header | The application believes `X-Forwarded-For` only from proxies it is configured to accept (03 §12). |
| **Caddy / nginx** | HTTP servers | often used as reverse proxies; Caddy obtains TLS certificates automatically | |
| **access log** | log | an HTTP server's record of each request it received | |
| **JSON API** | API | answers HTTP requests with JSON instead of HTML | |
| **CDN (content delivery network)** | set of servers | serves copies of websites' files from many places | A page that loads scripts from a CDN trusts that CDN. |
| **Cloudflare** | organisation | runs a CDN and reverse-proxy service in front of many websites | |
| **HSTS** | HTTP response header | tells the browser to use only HTTPS for the site for a stated time | |
| **robots.txt / security.txt** | files at fixed URLs | `/robots.txt` tells automated visitors which paths to avoid; `/.well-known/security.txt` (RFC 9116) gives an address for sending news of security problems | |

---

## §3 BitTorrent content

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **content** | set of files | offered together for sharing, in a fixed order | This project **never** downloads content. |
| **BitTorrent** | file-distribution protocol | participants exchange fixed-size, hash-verified slices of shared content directly with each other | |
| **BEP** | specification | a numbered *BitTorrent Enhancement Proposal*, published at bittorrent.org, that defines one part of BitTorrent | BEP 0 gives each BEP a status: Final, Active, Accepted, Draft, Deferred, Withdrawn or Rejected. BEP 3 defines BitTorrent itself. |
| **bencode** | serialization format | has exactly four value types: **byte string** `<len>:<bytes>`; **integer** `i<n>e`; **list** `l…e`; **dictionary** `d…e`, holding pairs of a byte-string key and a value | BEP 3. BEP 3 also requires dictionary keys in ascending raw-byte order and integers without leading zeros or `-0`. Canonical bencode follows those rules and has no duplicate keys (implied by the order rule, not stated). Every BitTorrent structure in this project is bencoded. |
| **BitTorrent v1 / v2 (v1, v2)** | editions of BitTorrent's description of content | **v1** (BEP 3) cuts the concatenation of all files into equal slices and hashes each slice with SHA-1; **v2** (BEP 52) cuts each file separately and hashes its slices into a SHA-256 Merkle tree | "v1" elsewhere can also mean this project's first published program (0.1) or the first edition of its JSON API (`/api/v1`); §8.5 sorts out these senses. |
| **piece** | byte string | one of the equal slices that v1 or v2 cuts content into; only the final slice of the content (v1), or the final slice of each file (v2), may be shorter | |
| **piece length** | integer | the number of bytes in each full piece | In v2 it must be a power of two and at least 16 KiB (BEP 52). In v1 it is almost always a power of two, but BEP 3 does not require it. |
| **info dictionary** | bencoded dictionary | describes the content: always `name` and `piece length`; for v1, `pieces` (the concatenated SHA-1 digests) and exactly one of `length` or `files`; for v2, `meta version` = 2 and `file tree`. Some have both sets. | Optional keys include `private` and UTF-8 variants such as `name.utf-8`. `meta version` is a format number (see §8.5). |
| **file entry** | dictionary inside an info dictionary | describes one file: its `length` and `path` list in v1's `files`, or its leaf in v2's `file tree` | Each element of a `path` list is one path component. |
| **padding file** | file entry | its file contains only zeros and exists only to align the next file with a piece boundary | BEP 47: `attr` contains `p`, and BEP 47 recommends the path `.pad/<N>`. Older clients used names like `_____padding_file`. Hidden from listings and from displayed totals. |
| **metadata** (BEP 9 sense) | byte string | the raw span of the info dictionary | |
| **infohash (v1)** | digest | SHA-1 of the metadata; 20 bytes | Because SHA-1's collision resistance is broken, someone can make two info dictionaries with the same infohash, but only if they create both. Forging an info dictionary that matches someone else's infohash would need a second preimage, which nobody can find. |
| **infohash (v2)** | digest | SHA-256 of the metadata; 32 bytes | BEP 52. |
| **truncated infohash** | byte string | the first 20 bytes of a v2 infohash | Used for v2 wherever a 20-byte key is required. |
| **DHT key** | 20-byte byte string | a v1 infohash or a truncated v2 infohash | It is the lookup key that BitTorrent's DHT uses (§4). A 20-byte key alone does **not** say whether it is v1 or v2, so the check in §5 accepts either. |
| **metainfo file (.torrent)** | bencoded dictionary saved as a file | holds the info dictionary plus outer fields: optional `announce` and `announce-list` (lists of URLs), `comment`, `creation date` and `encoding` (the name of a pre-Unicode text encoding); and `piece layers`, which BEP 52 requires for v2 | |
| **torrent** | content | described by one info dictionary and identified by its infohash or infohashes | In everyday speech "torrent" also means the metainfo file. These documents never use it that way. |
| **peer wire protocol** | application protocol over TCP | the BEP 3 message set for requesting and sending pieces of a torrent | |
| **peer** | process | exchanges pieces of a torrent with other processes over the peer wire protocol | |
| **peer ID** | 20-byte byte string | chosen by a peer to identify itself within one connection | |
| **swarm** | set of peers | exchanging pieces under the same infohash | |
| **hybrid torrent** | torrent | its info dictionary has both v1 and v2 fields describing the same content, so it has two infohashes and two swarms | |
| **seeder / leecher** | peer | a **seeder** has every piece; a **leecher** lacks at least one | |
| **BitTorrent client** | program | takes part in swarms as a peer on a person's behalf, and usually also in BitTorrent's DHT (qBittorrent, Transmission, Deluge…) | |
| **tracker** | server | tells BitTorrent clients the endpoints of other peers in a swarm, over HTTP or UDP | Its URLs are in the metainfo file's `announce` fields. BitTorrent's DHT can replace trackers. |
| **private torrent** | torrent | its info dictionary contains `private` = 1 (BEP 27) | Such a torrent may be registered only with its own trackers, never to BitTorrent's DHT. This project skips these. |
| **extension** | optional addition to a protocol | defined in its own BEP; participants that do not implement it keep working | The Draft BEPs used here are the DHT extensions 32, 42, 43, 45 and 51, plus BEP 47 (padding files) and BEP 52 (the v2 format, which is not a DHT extension). All are widely implemented. |
| **magnet link** | URI | identifies a torrent by infohash so that a client can get the metadata from peers: `magnet:?xt=urn:btih:<40 hex or 32 base32>` (v1) and/or `xt=urn:btmh:1220<64 hex>` (v2, a multihash); optional `dn` (display name) and `tr` (tracker URL) | Only `xt` is required (BEP 9). This project makes magnet links from validated hashes, never from raw user input, and percent-encodes `dn`. |
| **libtorrent** | library | the C++ BitTorrent library (libtorrent-rasterbar) used by qBittorrent, Deluge and others; its DHT code is the de facto reference implementation | libtorrent has implemented BEP 32 and BEP 51 since libtorrent 1.2, and BEP 52 since libtorrent 2.0. |

---

## §4 The distributed hash table (DHT)

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **node** | process | a member of a DHT | Distinct from a *peer* (§3), which exchanges pieces over TCP. One BitTorrent client is usually both. |
| **key space** | set of numbers | all 160-bit numbers (2¹⁶⁰ values) | Every DHT key is one of them. |
| **node ID** | number in the key space | identifies one node and fixes its position relative to DHT keys | |
| **XOR distance** | function of two numbers in the key space | their bitwise exclusive-or, read as an unsigned integer | Smaller means closer. |
| **contact** | record | another node's ID and endpoint, as one node holds it | |
| **routing table** | data structure held by one node | records contacts, organised by the XOR distance of their IDs from the node's own ID | |
| **k-bucket** | part of a routing table | holds at most *K* contacts whose IDs fall in one range of the key space | BEP 5: *K* = 8. Only the bucket that covers the node's own ID may split in two when full. |
| **iterative lookup** | procedure | asks the closest known nodes about a target ID, then asks the closer nodes they name, and repeats until no closer nodes appear | Requests go out in parallel, a few (α) at a time. |
| **Kademlia** | DHT design | measures closeness by XOR distance, keeps contacts in k-buckets, and finds keys by iterative lookup | Maymounkov & Mazières, 2002. |
| **Mainline DHT** | Kademlia DHT | the one BitTorrent clients use (BEP 5): its keys are DHT keys, and the value under a key is the set of endpoints of peers in that key's swarm | "The DHT" in this project means this network. It is commonly estimated at about 10 million nodes. That figure is an estimate, not a measurement (Pubky, 2024); BEP 42 estimated about 8.4 million around 2014. |
| **transaction ID** | short byte string | chosen by the sender of a request and copied into the reply, so the sender can match the two | |
| **KRPC** | application protocol | the Mainline DHT's remote-procedure-call protocol: every message is one UDP packet holding a bencoded dictionary with `t` (the transaction ID), `y` (the message's kind: `q`, `r` or `e`) and optionally `v` (a short string naming the sending software) | BEP 5. There are no retries at the protocol level. |
| **KRPC query** | KRPC message | `y` = `q`; names a method in `q` and its arguments in `a`, and expects exactly one response or error | In §4 and §5, "query" alone means a KRPC query. |
| **KRPC response** | KRPC message | `y` = `r`; answers one query, with its results in `r` | |
| **KRPC transaction** | exchange | one KRPC query together with its response or error, matched by transaction ID and remote endpoint | |
| **announce token** | opaque byte string | issued by a node to the IP address of a querier; the node later accepts it back only from that IP address and only for a limited time | Stops a node from announcing an IP address it does not own. BEP 5 suggests SHA-1(IP + secret), with the secret changed every 5 min and each announce token accepted for 10 min. BEP 5 calls it simply a "token". |
| **KRPC error** | KRPC message | `y` = `e`, with `e` = [code, text]. Codes: 201 generic, 202 server, 203 protocol (malformed packet, invalid arguments, bad announce token), 204 method unknown | |
| **ping** | KRPC query | asks only "are you alive?" and carries only the sender's ID | |
| **find_node** | KRPC query | asks for the nodes closest to a `target` ID; the response carries them in `nodes` | |
| **get_peers** | KRPC query | asks for peers of an `info_hash`. The response carries an announce token (`token`) plus `values` (peer endpoints) and/or `nodes` (closer nodes) | |
| **announce_peer** | KRPC query | tells a node "I am a peer of this `info_hash` at this `port`", and must carry an announce token that node issued to the same IP address. `implied_port` = 1 means "use my UDP source port". | The node stores the announcement and returns it in later get_peers responses. |
| **sample_infohashes** | KRPC query (BEP 51) | asks a node for a **sample** of the DHT keys it stores. The response has `samples` (N × 20 bytes), `num` (how many keys it stores), `interval` (0–21 600 s to wait before asking that node again) and `nodes` | The approved way to catalogue the DHT's keys. BEP 51 says one node can survey the whole DHT in a few hours "without having to resort to non-compliant behavior". Its rationale says passive get_peers harvesting "incentivizes bad behavior such as spoofing node IDs and attempting to pollute other nodes' routing tables". libtorrent answers an unknown query that carries `target` or `info_hash` as if it were find_node, so a response without `samples` means the node does not support BEP 51. |
| **good / questionable / bad node** | classification of a contact | **good**: answered one of our queries in the last 15 min, or has answered before and sent us a query in the last 15 min. **questionable**: 15 min without such activity. **bad**: failed to answer several queries in a row | BEP 5. Bad nodes are replaced. |
| **replacement cache** | list attached to a k-bucket | holds candidate contacts to insert when a member turns bad | Despite the name, it stores candidates, not saved results. |
| **bucket refresh** | procedure | an iterative find_node lookup for a random ID in a bucket's range, done when the bucket has not changed for 15 min | BEP 5. |
| **compact node info** | byte string | one contact in fixed-size binary form: 20-byte ID + 4-byte IPv4 address + 2-byte port = **26** bytes, or 20 + 16-byte IPv6 address + 2 = **38** bytes | A list whose length is not a multiple of 26 (or 38) is invalid. |
| **compact peer info** | byte string | one peer endpoint: 4 + 2 = **6** bytes (IPv4) or 16 + 2 = **18** bytes (IPv6) | |
| **bootstrap node** | node | listens at a well-known endpoint, usually given by a domain name (e.g. `dht.libtorrent.org:25401`), that a new node contacts first to learn about other nodes | Checked live on 2026-09-16: `dht.libtorrent.org:25401`, `dht.transmissionbt.com:6881` and `router.bt.ouinet.work:6881` answered; `router.bittorrent.com` and `router.utorrent.com` did not. |
| **bootstrapping** | procedure of a node | fills an empty routing table by an iterative lookup of its own ID, starting from bootstrap nodes | |
| **external IP** | IP address | the address other hosts see for a node, which may differ from its local address because of NAT | This project learns it by a vote over the `ip` field in responses to its own queries. Each IPv4 /24 or IPv6 /48 casts at most one vote, votes expire after 30 min, and a winner needs at least 10 votes and more than two-thirds of current votes (03 §7). |
| **secure node ID (BEP 42)** | node ID | its top 21 bits equal the top 21 bits of CRC32C over the node's masked external IP address, where the mask is `03 0f 3f ff` over the 4 bytes of an IPv4 address or `01 03 07 0f 1f 3f 7f ff` over the first 8 bytes of an IPv6 address, and `r << 5` is ORed into the first masked byte, with `r` equal to the low 3 bits of the ID's last byte | Makes it hard to choose an arbitrary ID. CRC32C is used to spread IDs evenly, not for security. The BEP's prose (hash 8 bytes, as a big-endian 64-bit integer, for both families) contradicts its own example code and worked examples (its "test vectors"). This project follows the example code and the worked examples: 4 bytes for IPv4, 8 for IPv6. |
| **IPv6 DHT extension (BEP 32)** | extension | runs a separate IPv6 Mainline DHT with its own routing table, adds `nodes6` to responses and a `want` (`n4`/`n6`) argument to queries, and limits packets to 1 024 bytes of payload | BEP 32 recommends one node ID for both address families. This project uses one per family, because BEP 42 derives each ID from that family's address (03 §7). |
| **read-only node** | node | does not answer queries and marks its own queries with `ro` = 1 (BEP 43) | Other nodes must not add it to their routing tables. |
| **DHT scrape (BEP 33)** | extension of get_peers | `scrape` = 1 asks for two 256-byte Bloom filters (`BFsd` seeders, `BFpe` peers) from which the swarm's size can be estimated | Optional. Deferred in this project. |
| **multi-address rules (BEP 45)** | extension | when one host runs nodes on several addresses, each node gets its own well-separated ID, and replies leave from the socket the query arrived on | |
| **Sybil attack** | attack on an overlay network | one adversary runs many identities to gain influence out of proportion to its real size | |
| **horizontal / vertical Sybil** | Sybil attack | **horizontal**: the identities' IDs are spread evenly across the key space, so together they sit near every key; **vertical**: they cluster around one key | |
| **node-ID spoofing ("neighbour trick")** | misuse of node identity | a node picks, or keeps changing, its node ID so that it sits next to other nodes' IDs or target keys and receives traffic meant for them | Done with many identities at once, it becomes a Sybil attack. BEP 51 exists to make it unnecessary. BEP 42 limits its effect. |
| **routing-table pollution** | harm | contacts that do not represent honest, stable nodes (spoofed, duplicated or unresponsive) fill other nodes' routing tables | |
| **eclipse attack** | attack | surrounds a victim node with contacts the adversary controls, so the victim's view of the network is false | |
| **rate limit** | control (security) | caps how many actions happen per unit of time | libtorrent bans a sender for 5 min if it sends more than 5 packets/s to it. |
| **token bucket** | rate-limit algorithm | keeps a count of permits (the "tokens" of its name) that refills at a fixed rate up to a cap; each action spends one permit, so short bursts are allowed and the average rate is bounded | These permits are not announce tokens. |
| **overflow bucket** | token bucket | shared by all new clients once a rate limiter's LRU map of buckets is full, so live entries are not evicted | 03 §12. |
| **responder budget** | budget | the packets and bytes per second a node may spend on answering queries, kept separate from its budget for sending its own queries; queries beyond it are dropped unanswered | 03 §3: 500 packets/s and 64 000 bytes/s. The query budget defaults to 250 packets/s. |
| **peer store** | map held by a node | maps DHT keys to the peer endpoints it received in announce_peer queries, returns them in get_peers responses, and expires them | In this project it is bounded, in memory only, and never saved. |
| **good DHT citizen** | node | follows the DHT's written rules (BEP 5 responder duties, BEP 42 node IDs, BEP 43 `ro`, BEP 45 one ID per socket address, BEP 51 `interval`) and its unwritten norms (stays under the per-IP rates libtorrent enforces, and takes no more than it gives) | 01 §5 says "compliant DHT citizen" with the same meaning. A design goal of this project. |
| **sampler** | component of a node | sends sample_infohashes queries to discover DHT keys. Its **frontier** is the bounded set of nodes to ask next; its **visited map** records the earliest time each node may be asked again | 03 §7. |
| **address chokepoint** | component | the single filter that every address must pass before this project's code connects to it or adds it to a routing table; it rejects special-purpose addresses and port 0 | Implemented as `is_dialable` (03 §7). IPv4-mapped and IPv4-compatible addresses are first converted to IPv4. |

---

## §5 Fetching metadata from peers

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **handshake** | peer wire message | the first 68 bytes of a connection: byte 19 (`pstrlen`), `"BitTorrent protocol"` (`pstr`), 8 **reserved** bytes, the 20-byte DHT key (a v1 infohash or a truncated v2 infohash), and a 20-byte peer ID | Both sides must name the same key. |
| **message framing** | rule | every later message is a 4-byte big-endian length, then the message (a 1-byte ID and a body). Length 0 is a **keep-alive**. | A bounded parser rejects lengths above a limit **before** allocating memory. |
| **bitfield message** | peer wire message | lists which pieces the sender has, one bit per piece | Its size grows with the number of pieces (03 §3). |
| **extension protocol (LTEP, libtorrent extension protocol)** | extension of the peer wire protocol | message ID 20 carries extension messages; extended ID 0 is the **extended handshake**, a bencoded dictionary whose `m` maps extension names to the sender's message IDs | BEP 10. Each side chooses its own IDs, so send using the IDs the *other* side advertised. |
| **reserved bits** | flags in the handshake | mark optional features: `reserved[5] & 0x10` = extension protocol (BEP 10); `reserved[7] & 0x01` = Mainline DHT support (BEP 5) | |
| **ut_metadata** | extension (BEP 9) | transfers the metadata in **16 KiB (16 384-byte) metadata pieces**. The extended handshake gives `metadata_size`. Messages are dictionaries with `msg_type` 0 = request, 1 = data (plus `total_size`, followed by the raw piece bytes), 2 = reject. | The BEP's reject example wrongly shows `msg_type` 1; the prose (2) is followed. Every piece except the last must be exactly 16 KiB. BEP 9 requires a peer without the full metadata to reject requests. |
| **pipelining (requests)** | request pattern | sends several requests before the earlier answers arrive | `reqq` in the extended handshake says how many a peer accepts. |
| **metadata verification** | check | accepts fetched metadata only if SHA-1(bytes) equals the DHT key, **or** the first 20 bytes of SHA-256(bytes) equal it | Same rule as libtorrent. After verification the bytes are accepted as *being* that torrent's metadata, but their *contents* are still written by strangers. |
| **traffic shaping** | network practice | an ISP slows or blocks traffic according to the protocol it recognises | |
| **MSE/PE (Message Stream Encryption / Protocol Encryption)** | extension | encrypts the peer wire stream so that traffic shaping cannot recognise it | Deferred (03 §15). |
| **uTP** | transport protocol (BEP 29) | a congestion-friendly alternative to TCP that runs over UDP | Deferred (03 §15). |

---

## §6 Search

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **search engine** | program or library | stores text records and returns, for a text request, the records that match it, ranked | |
| **document (search)** | record | the unit a search engine stores and returns | Here, one torrent's name and file paths. |
| **corpus** | set of documents | the set a search engine searches | |
| **token (search)** | piece of text | the unit a search engine matches (usually a normalised word) | Not an announce token (§4) or a token-bucket permit (§4). |
| **tokenizer** | component | splits text into tokens and normalises them (for example with NFKC, case folding or lowercasing, and accent folding) | |
| **query (search)** | request | asks a search engine for the documents that match some text | Other senses: KRPC query (§4), and a third sense in §7. |
| **index (search sense)** | data structure | built from a corpus in advance, so that a query is answered **without scanning every document** | Two other senses of "index" are in §7 and §8.6. |
| **field (search)** | named part of a document | indexed and searched on its own (here `name` and `files`) | |
| **term (search)** | pair | a field and a token; the unit a search looks up | 01 R17 and 03 §3 count words, not terms. |
| **inverted index** | index (search sense) | maps each term to its **posting list**, the documents (and the positions within them) that contain it | |
| **full-text search engine** | search engine | answers free-text queries using inverted indexes | It is either a library linked into a program or a server of its own. |
| **Lucene** | full-text search engine | a Java library, the basis of Elasticsearch and Solr | |
| **Tantivy** | full-text search engine | a Rust library modelled on Lucene | This project links it into its binary. |
| **Bleve** | full-text search engine | a Go library | |
| **Sphinx** | full-text search engine | a standalone C++ server that creates its indexes from an `xmlpipe2` XML stream | |
| **Meilisearch** | full-text search engine | a standalone server written in Rust | |
| **CJK** | group of writing systems | those of Chinese, Japanese and Korean. Chinese and Japanese are written without spaces between words. Korean puts spaces between word units, but each unit joins a stem with particles, so whitespace tokens rarely match the query word. | A whitespace tokenizer fails on all three. |
| **Han / Hiragana / Katakana / Hangul** | writing systems | **Han**: the Chinese ideographs, also used in Japanese and Korean; **Hiragana** and **Katakana**: the two Japanese syllabaries; **Hangul**: the Korean alphabet | |
| **word segmentation** | tokenization method | uses a lexicon (a word list) to find word boundaries in text without spaces | Needs a lexicon per language. |
| **n-gram (bigram, unigram)** | tokenization method | emits every run of *n* consecutive characters as a token (a bigram has *n* = 2, a unigram *n* = 1) | Needs no lexicon and works for every CJK writing system. Lucene's CJK analyzer uses bigrams. This project indexes CJK text as overlapping bigrams, and also indexes every CJK character as a unigram in separate fields, so a one-character query matches too (03 §11). |
| **phrase query / prefix query** | query (search) | a **phrase query** requires its tokens in consecutive positions; a **prefix query** matches every token that starts with a given text | Prefix matching lets a query match as the user types (`ubunt` matches `ubuntu`). Each prefix *expands* into the matching terms, and 03 caps the expansions. |
| **field options** | settings of a field | an *indexed* field can be searched; a *stored* field is returned with results; a *fast* field is kept in a column that can be read quickly per document for sorting and scoring; *positions* record where each token occurs, which phrase queries need | |
| **relevance** | measure | how well a document matches a query | |
| **BM25** | relevance formula | scores a document higher when query tokens appear in it often, when those tokens are rare in the corpus, and when the document is short | The standard in Lucene and Tantivy. A **boost** multiplies one field's score. |
| **signal** | measured property of a document | used besides relevance to order results | |
| **ranking** | ordering | sorts results by relevance, optionally adjusted by signals | |
| **popularity (here)** | signal | how many deduplicated sightings this project has recorded for a DHT key (at most one per key in any 30 minutes), from BEP 51 samples and from the announce_peer and get_peers queries its node received | Measures *interest in the DHT*, not quality. 03 §10 stores it as `seen_count`. |
| **recall** | measure | the fraction of matching documents that a search actually returns | |
| **pagination** | technique | splits results into pages. The **offset** is how many results to skip | Deep offsets are expensive, so they are capped. |

---

## §7 Storage and processing

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **database** | organised collection of durable data | built to be read and changed by many processes at once through one managing program | |
| **database query** | request | asks the program that manages a database to read or change data | |
| **database server (DBMS)** | program | manages one or more databases and answers database queries from other processes | 01 R12 ("the database is reachable only on an internal network") means the server. |
| **SQL** | query language | the standard declarative language for data kept as **tables** of **rows** with typed **columns** | |
| **relational database** | database | organised as tables of rows with typed columns, and queried with SQL | |
| **PostgreSQL** | database server | an open-source relational database server | This project uses PostgreSQL 18 (image `postgres:18.6`). Its `jsonb` column type stores JSON in a parsed binary form, and large values are compressed, with lz4 where the server supports it. |
| **primary key (PK)** | column or set of columns | uniquely identifies each row of a table | |
| **index (database sense)** | data structure inside a database | speeds up finding rows by the values of some columns (B-tree, GIN…) | A *unique* index also forbids two rows with the same value. Not the same as a search index (§6). |
| **schema** | set of definitions | a database's tables, columns, types and indexes (or a search index's fields) | |
| **migration** | numbered file of SQL statements | moves a database schema from one state to the next; migrations are applied in order, and each is recorded once applied | |
| **transaction (database)** | group of database operations | either all take effect or none do | Other sense: KRPC transaction (§4). |
| **commit (database / index)** | final step of a transaction or of an index write | makes all of its changes durable and visible at once | The other sense is in §8.1. |
| **rollback** | final step of a transaction | discards all of its changes | A *finished* transaction is one that has committed or rolled back. |
| **lock (database)** | claim held by a transaction | on a row, a table or another object, so that conflicting transactions wait | `FOR UPDATE SKIP LOCKED` locks rows and skips those another transaction holds. `lock_timeout` limits the wait; `idle_in_transaction_session_timeout` ends sessions that sit idle inside a transaction. |
| **advisory lock** | lock (database) | named by a number chosen by the application and not tied to any row; it can be shared or exclusive | A transaction-level advisory lock (`pg_advisory_xact_lock…`) is dropped automatically at commit or rollback. |
| **sequence (database)** | database object | hands out increasing integers (`nextval`) | Numbers taken by transactions that roll back are never reused, so gaps are normal. `CACHE 1` makes each session take one number at a time. `last_value` is the latest number handed out. |
| **database role** | database account | a named set of privileges that a connection logs in as | Other senses: client/server roles (§2.1), and a third sense in §8.6. |
| **database function** | subroutine | stored in a database and run by the database server | |
| **SECURITY DEFINER function** | database function | runs with the privileges of its owner instead of its caller's, so a caller can do exactly what the function does and nothing more | Used for least-privilege roles, where each role can execute only what it needs. |
| **bound parameter** | value in a database query | sent separately from the SQL text, so it can never be read as SQL | |
| **connection pool** | set of open database connections | shared and reused by a program's tasks | |
| **upsert** | database write | inserts a row, or changes it if the key already exists | |
| **idempotent** | property of an operation | applying it twice has the same effect as applying it once | Lets a failed step be retried safely. |
| **data store** | component | keeps data so it can be read later (a database, a search-index directory, a file) | |
| **source of truth** | data store | its contents are authoritative; every other copy is derived from it | Here: PostgreSQL. |
| **derived projection** | data store | built entirely from the source of truth, so it can be deleted and rebuilt | Here: the Tantivy index. |
| **hydrate** | action | fills search results (document IDs) with current data from the source of truth | |
| **tombstone** | row | marks an item as deleted while keeping its key, so the deletion itself reaches derived projections | |
| **change feed** | ordered stream of rows | the rows changed after a given sequence number, read in the order of a sequence column (here `change_seq`) | |
| **high-water mark** | sequence number | chosen so that every sequence number at or below it belongs to a finished (committed or rolled-back) transaction | A reader that stops at the mark skips no committed change. Gaps below it are expected. 03 §10 takes it under an exclusive advisory lock. |
| **checkpoint** | stored value | records how far a consumer has read an ordered stream, so it can resume there | |
| **generation (search index)** | complete copy of a search index | built in its own directory; a `CURRENT` file names the live one | A rebuild writes a new generation while the old one keeps serving, then switches `CURRENT` atomically. Each generation keeps its checkpoint in its commit payload, and one writer at a time holds its *writer lock*. |
| **cache** | data store | holds recent results so they need not be recomputed | |
| **negative cache** | cache | remembers recent **failures** so they are not retried too soon | |
| **torrent cache (website)** | website | stores metainfo files by infohash and serves them over HTTP to anyone (torcache.net, torrage.com) | Not a cache in the sense above. |
| **durable queue** | queue | kept in a database, so it survives process restarts | |
| **worker** | task | repeatedly claims items from a queue and processes them | |
| **lease** | time-limited claim | a worker holds on a queue item; if the worker dies, the claim expires and another worker may take the item | |
| **lease renewal** | action | a worker that still holds an item extends its lease before it expires, so that slow work is not taken over by another worker | 03 §13: leases last 120 s and are renewed after 90 s. |
| **at-most-once / at-least-once** | delivery guarantees | **at-most-once** may lose an item but never repeats it; **at-least-once** never loses an item but may repeat it | The old system deleted items before processing them (at-most-once). This project uses leases (at-least-once) with idempotent writes. |
| **backoff (exponential)** | retry policy | the wait before each retry grows by a constant factor, up to a cap | |
| **pipeline** | chain of **stages** | each stage processes items and passes them to the next | |
| **backpressure** | flow-control behaviour | a slow stage makes faster stages before it wait, instead of letting work pile up without bound | |
| **load shedding** | flow-control behaviour | a stage that cannot keep up discards excess work (and counts it) instead of queueing it or making the sender wait | |
| **bounded channel** | queue between concurrent tasks | has a fixed capacity, so a full channel either makes the sender wait (backpressure) or refuses the item (load shedding), depending on how the item is sent | 03 §7 uses load shedding for discoveries. |
| **semaphore** | counter of permits | lets at most *N* tasks hold a permit at once; the others wait | |
| **byte budget** | semaphore | its permits are bytes, so the total size of the data held in flight is bounded | 03 §3: in-flight metadata, default 256 MiB. |
| **blocking pool** | set of threads | runs work that would otherwise stall the asynchronous runtime (here, searches) | |
| **dedup set** | set of recently seen keys | kept in two **generations**: new keys go into the current one, and at each rotation the older generation is dropped and an empty one begins | With rotation every 30 min, a key is remembered for 30–60 min. Not the same sense of *generation* as a search-index generation. |
| **metric** | number | measured and exported over time: a *counter* (only rises), a *gauge* (rises and falls) or a *histogram* (a distribution) | A *label* splits one metric by a dimension (e.g. `family`). |
| **Prometheus** | monitoring system | collects metrics over HTTP in its text format and evaluates alert rules | |
| **health check** | HTTP route | reports whether a process is alive (*liveness*, `/healthz`) and able to do its work (*readiness*, `/readyz`) | |
| **audit log** | append-only record | who performed each governance action, when, on what, and why | |

---

## §8 The words in the request

The request was: *"update this repo … all websites using btdig have been compromised
… build a better version."* Each word gets the same treatment. The method's own
vocabulary (cause, function, axiom) is defined here too.

### §8.1 Repositories

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **git** | program | keeps a directory's files together with their full change history, as a chain of recorded snapshots identified by SHA-1 digests | |
| **repository (repo)** | data store | a directory tree and its change history, as git keeps them | |
| **commit (git)** | repository object | one recorded snapshot of the files, with its author and a message | |
| **branch** | name in a repository | moves forward to the latest commit of one line of work | `HEAD` is the latest commit of the current branch. "Pinned to `HEAD`" means "whatever the latest commit is at fetch time". |
| **tag** | name in a repository | fixed on one commit, usually to mark a published program | |
| **GitHub** | website | hosts git repositories, with shared accounts for groups and automated check services | |
| **issue tracker** | part of a code-hosting website | where users report problems with a project and discuss them | |
| **GitHub organisation** | shared GitHub account | owns repositories for a group; *verified* means it has proved control of a domain name | |
| **archived repository** | repository | made read-only by its owner | |
| **clone (repository)** | copy of a repository | kept locally | |
| **fork (repository)** | copy of a repository | published under another owner | |
| **upstream** | repository | the original that a fork or clone was made from | |
| **fork (project)** | software project | develops a copy of another project's code independently | |

### §8.2 Crawlers and search engines

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **crawler** | program | systematically visits the parts of a network to discover what exists there, and records it | A *web* crawler visits web pages; a *DHT* crawler visits DHT nodes. |
| **DHT crawler** | crawler | the network it visits is the Mainline DHT, and what it records is DHT keys (and, after fetching, their metadata) | |
| **DHT search engine** | search engine | its corpus is the metadata of torrents discovered through the Mainline DHT, not web pages | §8.3 and §8.6 define the two this project is about. |
| **bitmagnet** | DHT search engine | open source and written in Go | 01 §6 compares it. |

### §8.3 The legacy system

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **Kevin Lynx** | person | the developer whose DHT library and search engine this project replaces | |
| **Erlang** | memory-safe language | runs on the BEAM virtual machine as many lightweight Erlang processes, which are not operating-system processes; a `.beam` file is one compiled module | R16 dates from 2013. Each Erlang process has a *PID*. |
| **OTP** | library | Erlang's standard framework, including supervisors and the `inets` HTTP server, whose `mod_esi` maps URLs to module subroutines | |
| **supervision tree** | tree of Erlang processes | supervisors restart failed children, and too many restarts in a period stop the supervisor and everything under it | |
| **atom (Erlang)** | constant | stored once in a VM-wide atom table that is never garbage-collected and has a fixed limit (1 048 576 by default), so creating atoms from input can crash the VM | |
| **NIF (native implemented function)** | subroutine | C or C++ machine code loaded into the Erlang VM's process; a bug in it can crash or corrupt the whole VM | |
| **`export_all`** | compiler directive | makes every subroutine in an Erlang module callable from outside | |
| **`binary_to_term`** | Erlang subroutine | decodes Erlang's external term format; without the `[safe]` option it creates new atoms from its input | |
| **abstract code** | Erlang parse tree | kept in a `.beam` file's `debug_info` section | To *decompile* is to turn compiled code back into readable source. |
| **semantically identical** | relation between a compiled module and a source file | their abstract code is equal once line numbers and compiler metadata are ignored | 02 §4. |
| **rebar** | program | Erlang's tool for compiling projects and fetching the dependencies listed in `rebar.config` | |
| **BSON** | serialization format | a binary form of JSON-like records | |
| **MongoDB** | database server | stores records (called "documents", at most 16 MiB each) in BSON | 2.4 dates from 2013. A MongoDB "document" is not a search document. |
| **replica set** | group of MongoDB servers | keeps copies of the same data; a shared *keyFile* secret authenticates the members to each other | |
| **coreseek** | fork (project) | of Sphinx, adding Chinese word segmentation | |
| **dhtcrawler2** | DHT search engine | written in Erlang by Kevin Lynx in 2013: MongoDB storage, Sphinx or MongoDB text search, and `.torrent` files downloaded over plain HTTP from third-party torrent-cache websites | It used MongoDB 2.4. Archived upstream since 2020. See [02-legacy-audit](02-legacy-audit.md). |
| **legacy** | property | belongs to dhtcrawler2, the system being replaced | 01 §4. |
| **rmmseg** | word-segmentation library | a Chinese MMSEG segmenter (rmmseg-cpp) | dhtcrawler2 could use it, as a non-default option, through a Windows DLL shipped without source. The DLLs' debug paths point to a locally compiled copy of rmmseg-cpp (02 §4). |
| **kdht** | library | Kevin Lynx's Erlang Mainline DHT node, used by dhtcrawler2 | |

### §8.4 btdig and what "compromised" means

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **compromised** | state of a system | a party other than its legitimate operator can make it act against the interests of its operator or its users. The result is a loss of **confidentiality**, **integrity** or **availability** that the operator did not choose. | An outage, or a harmful decision by the operator, does not make a system compromised in this sense. |
| **compromise** | event | a system becomes compromised | |
| **cloaking** | deception technique | serves different responses depending on who appears to be asking (for example the `User-Agent`, `Accept` and `Sec-Fetch-Dest` headers, and possibly the country) so that scanners see a clean page | |
| **traffic distribution system (TDS)** | redirect service | receives hijacked or bought visitors and forwards them to scam, adware or malware destinations | |
| **btdig** (sense 1: the service) | DHT search engine | runs at `btdig.com` under the name "BTDigg", does not publish its code, and claims continuity with the original BTDigg (`btdigg.org`, 2011–2016) | Whether it really continues BTDigg is disputed. Verified on 2026-09-16: since about 2026-07-10, `btdig.com` sends browser-like visitors to scam and advertising domains with hidden redirects, and hides this from scripts. That is cloaking. The header dependence was reproduced; the country dependence is reported only by users. It is not known whether an attacker or the operator added the redirects. If a third party did, btdig.com is compromised. If the operator did, it is not compromised but is acting against its users. Either way it **acts against its users**, and that is what the design must guard against. |
| **btdig** (sense 2: the GitHub organisation) | GitHub organisation | verified for `btdig.com`; hosts the fork of dhtcrawler2 this repository was cloned from | The fork differs from upstream only in its README. Its only publicly listed member account now returns 404, which suggests suspension. |
| **btdig** (sense 3: "sites using btdig") | set of websites | run code derived from dhtcrawler2, embed or proxy btdig.com, or use its name | No public evidence was found that any such third-party site was compromised. |

### §8.5 Cause, function, version, better

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **cause (*aitia*)** | answer to "why is it so?" | one of four: **material** (what a thing is made of), **formal** (its structure), **efficient** (what brings it about), **final** (what it is for, its *telos*) | Aristotle, *Physics* II.3. 01 §1 applies them. |
| **function (*ergon*)** | characteristic activity of an artifact | the activity whose performance makes the artifact the kind of thing it is | The end that this activity serves (*telos*) is the artifact's final cause. Defined for this system in [01-first-principles](01-first-principles.md). |
| **capability** | activity of a part of a system | one that the system must perform in order to perform its function | 01 §3 calls its F1–F9 "functions"; they are capabilities in this sense. |
| **build** (sense 1) | act of making | produces a software artifact by writing its source code | |
| **build** (sense 2) | automated procedure | turns source code into binaries or other files ready to run, called **build artifacts** | A **reproducible build** gives bit-identical artifacts from the same source, so anyone can check a published binary. |
| **release** | build artifact set | published under a number (Rust 1.98.1, PostgreSQL 18.6) | This project's first release is 0.1. |
| **version** | artifact | a member of a line of artifacts, each made to replace the one before it and continuing its identity (its name, repository or declared lineage), that share one function (final cause) | The program this project builds is a version of dhtcrawler2 because it is made to replace it, in the same repository, and keeps its function, not because it shares code. bitmagnet has the same function but is not a version of dhtcrawler2. The everyday sense "release number" (`meta version`, "an exact version", "later MongoDB versions") is *release*. "v1" may mean BitTorrent v1 (§3), the first release, or the first edition of the JSON API. |
| **update** | change | applied to an existing repository: its contents are replaced by a new version of the program it holds, while the repository and the program's function persist | Here the repository is updated and its contents become the new program. The material (Erlang, MongoDB, HTTP caches) is replaced entirely and the function is kept, like the ship of Theseus. |
| **better** | comparative relation between two things of the same kind | one performs the function of that kind **more fully** and **more reliably**, and **causes less harm** in doing so | The first two criteria come from Aristotle's function argument (*Nicomachean Ethics* I.7): for a flute-player or a sculptor, the good lies in performing the function well. (The knife example is Plato's, *Republic* I 353a.) The third criterion, less harm, comes from 01 §2 (A3 and A7): a search engine that works well while harming its users or the DHT is not what the request means by better. So a "better version" is a DHT search engine that finds more torrents, more accurately and more safely. The measurable criteria are in 01 §5. |
| **first principles** | propositions | accepted at the start of an inquiry without being demonstrated in it: definitions, and propositions that are self-evident or directly observed | Aristotle, *Posterior Analytics* I.2 and *Metaphysics* Δ.1 (*archē*). Aristotle counts definitions among them because they are not demonstrated. |
| **axiom** | first principle | not a definition: a proposition accepted without demonstration because it is self-evident or directly observed | 01 §2 lists A1–A9, which rest on the definitions above. Axioms that cite 02 rest on observation, not demonstration. |
| **requirement** | proposition | states what the system must or must not do, and is derived from axioms and definitions | 01 §4 lists R1–R20. |

### §8.6 The new system

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **dhtcrawler3** | DHT search engine | the Rust rewrite of dhtcrawler2 specified in 01–03: it discovers DHT keys through its own Mainline DHT node (BEP 5, BEP 51), fetches metadata only from peers (BEP 9) and verifies it, stores it in PostgreSQL, and answers searches from an embedded Tantivy index | A version of dhtcrawler2. |
| **service role** | mode of the dhtcrawler3 binary | one of `crawl`, `index` or `web`, chosen by subcommand | `all` runs the three roles in one process. Each role logs in to PostgreSQL as its own database role. |
| **crawler role** | service role | `crawl`: runs the node, admits discovered keys, and fetches, verifies and stores metadata | Also written "the crawler". |
| **indexer role** | service role | `index`: reads the change feed and keeps the Tantivy index up to date | |
| **web role** | service role | `web`: serves the website and the JSON API | |

---

## §9 Security, law and governance

### §9.1 Trust and attack

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **this project's assets** | set of assets | the host, the stored data and index, the DHT's health, visitors' browsers, and visitors' privacy | |
| **trusted (component)** | property of a component | the system relies on it to behave correctly, so a fault in it can break the system's guarantees without being detected | |
| **trust boundary** | line in a system | data crosses it from a less-trusted to a more-trusted part | Every DHT packet, peer message, torrent name, HTTP request, and script loaded from another organisation crosses one. |
| **untrusted input** | data | comes from across a trust boundary and must be validated before use | **All** torrent metadata is untrusted: anyone can publish a torrent with any name. |
| **exploit** | method or program | uses a vulnerability | |
| **attack surface** | set of points | where an adversary can supply input to, or influence, a system | |
| **severity** | rating of a defect | Critical, High, Medium or Low, judged by impact and by how easily it can be reached | 02 §3 uses High, Medium and Low, as corrected by its skeptic pass. |
| **injection** | vulnerability class | untrusted input is interpreted as code or commands (SQL, shell, HTML, JavaScript) | |
| **cross-site scripting (XSS)** | injection | untrusted input becomes script in a web page. **stored**: the input was saved first (a torrent name); **reflected**: it comes straight from the request (a search keyword) | Both are present in dhtcrawler2. |
| **escaping** (OWASP: "output encoding") | transformation of text | rewrites special characters (`<`, `>`, `&`, `"`, `'`) so they display as text instead of being parsed as markup | |
| **template engine** | component | fills a fixed page layout with values to produce HTML; **auto-escaping** means it escapes every value unless told not to | |
| **`<bdi>` element** | HTML element | isolates the display direction of its text from the text around it | Stops bidi controls in a name from reordering the rest of the page. |
| **Content-Security-Policy (CSP)** | HTTP response header | tells the browser which sources of script, style, images and forms a page may use | A strict CSP stops injected script from running even when escaping fails. dhtcrawler3 pages use **no JavaScript at all**. |
| **security headers** | HTTP response headers | limit what a browser lets a page do: `X-Content-Type-Options` (no type guessing), `Referrer-Policy` (what URL is shared), `Permissions-Policy` (device features), `Cross-Origin-Opener-Policy` and `Cross-Origin-Resource-Policy` (isolation between origins), `X-Frame-Options` (no framing) | Listed in 03 §12. |
| **Histats / reCAPTCHA** | web services | a web analytics service / a bot check run by Google; pages use both by loading their scripts | btdig.com loads Histats (02 §2). |
| **third-party script** | script (web) | loaded by a page from a server run by another organisation (analytics such as Histats, reCAPTCHA, advertising) | It can do anything the page can do. |
| **CSRF (cross-site request forgery)** | attack | makes a visitor's browser send an unwanted request to a site | |
| **CORS (cross-origin resource sharing)** | browser mechanism | decides whether scripts from other origins may read a site's responses | Closed by default. |
| **SSRF (server-side request forgery)** | attack | makes a server send requests to a destination the adversary chooses, usually an internal one | |
| **decompression bomb** | DoS input | tiny when compressed but huge when expanded | dhtcrawler2's unbounded gunzip is exposed to it. |
| **stack overflow via recursion** | DoS | a deeply nested input makes a recursive parser exhaust its stack | Found by source review in two Rust bencode crates (serde_bencode 0.2.4, bt_bencode 0.8.2); the overflow was not reproduced. dhtcrawler3's parser is iterative and caps depth. |
| **supply chain** | set of things | everything a program is built from or depends on (libraries, compilers, build servers, and the operating-system files shipped with it) | |
| **supply-chain compromise** | compromise | enters through the supply chain rather than through the running system | |
| **lockfile** | file | records the exact release and cryptographic digest of every dependency (`Cargo.lock`) | |
| **dependency pinning** | practice | fixes each dependency to an exact release and a cryptographic digest of its source, recorded in a lockfile | Cargo.lock stores SHA-256 digests, which Cargo calls "checksums"; they are not checksums in the §1.6 sense. dhtcrawler2 pinned to git `HEAD`, which is the opposite. |
| **SBOM (software bill of materials)** | machine-readable list | names every component and dependency inside a build artifact, with their releases and digests | SPDX is one standard format. |
| **artifact signature** | digital signature | over a build artifact, showing that the holder of a key vouched for exactly those bytes | |
| **build attestation (provenance)** | signed statement | records which source commit, builder and steps produced a build artifact | GitHub artifact attestations are one kind; `gh attestation verify` checks them. |
| **least privilege** | design principle | each component gets only the permissions its function needs | |
| **defence in depth** | design principle | several independent controls, so that one failure does not become a compromise | |
| **fail closed** | design principle | on error, deny or stop rather than continue unsafely | |
| **MFA (multi-factor authentication)** | login method | needs a second proof besides a password; a *hardware key* is a physical device that provides it and resists phishing | |
| **synthetic check** | monitoring probe | a scheduled, scripted request from outside that imitates a real visitor and raises an alarm when the response differs from the expected page | |

### §9.2 Law and governance

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **EU / CJEU** | organisation / court | the European Union, and its Court of Justice, which gives binding interpretations of EU law | |
| **GDPR** | law | EU Regulation 2016/679, which governs how organisations handle information about people | |
| **personal data** | data | relates to an identified or identifiable natural person (GDPR Art. 4(1)) | The CJEU held that a dynamic IP address is personal data for a website operator that has legal means to identify the person through the ISP's data (*Breyer*, C-582/14, 2016), and repeated this in *Mircom* (C-597/19, 2021), a BitTorrent case. GDPR Recital 30 names IP addresses as online identifiers. dhtcrawler3 therefore treats peer and visitor IP addresses as personal data and never stores them. The only IP addresses it keeps are at most 300 DHT routing contacts, with no timestamps and no link to any key (01 R11). |
| **data minimisation** | principle (GDPR Art. 5) | collect and keep only what the purpose needs | |
| **communication to the public** | act under EU copyright law | making protected works available to a new public | The CJEU held that operating a torrent index can itself be one (C-610/15, *Stichting Brein v Ziggo*, 2017). |
| **DSA / Online Safety Act** | laws | the EU Digital Services Act and the UK Online Safety Act, which impose duties on online services | 04 §2 advises legal advice on both. |
| **CSAM (child sexual abuse material)** | illegal material | depicts the sexual abuse or sexual exploitation of a minor | A public index must block it from day one. |
| **NCMEC / CyberTipline** | organisation / reporting system | the US National Center for Missing & Exploited Children runs the CyberTipline; US providers must report apparent CSAM to it (18 U.S.C. §2258A) | |
| **IWF** | organisation | the Internet Watch Foundation, a UK charity; it gives its members lists of CSAM URLs and keywords | |
| **denylist** | list | names torrents that must be refused, by DHT key, v1 infohash or v2 infohash (20 or 32 bytes) | 03 §10 compares 20-byte prefixes, so a v2 entry also blocks its truncated key. |
| **blocked-term list** | list | words or phrases (here, CSAM indicators) whose presence as whole tokens in a name, a path or a query causes refusal | |
| **takedown** | action | removes an item from a service after a valid notice | |
| **safe harbour** | legal immunity | shields a service provider from liability for its users' infringement while it meets conditions set by law | |
| **DMCA notice** | legal notice (US, 17 U.S.C. §512) | sent by a copyright holder to a service provider to identify allegedly infringing material or links | Search tools (§512(d)) keep safe-harbour protection only if they meet all of its conditions: no actual or "red flag" knowledge of infringement; no financial benefit directly attributable to infringement they can control; prompt removal after a valid notice sent to a **designated agent** registered with the US Copyright Office (the registration expires after 3 years); and a repeat-infringer policy (§512(i)). |

---

## §10 Building and operating

### §10.1 Tests and automation

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **test** | program | runs part of a system and checks the result against an expectation | |
| **property test** | test | checks an invariant on many generated inputs (for example, decoding an encoded value gives the value back) | |
| **fuzzing** | testing technique | feeds large numbers of generated and mutated inputs to a component and watches for crashes, hangs or broken invariants | `cargo-fuzz` runs Rust fuzz *targets*, one per entry point. |
| **end-to-end test** | test | runs the whole pipeline, from input to stored and searchable output | |
| **test seeder** | component | a minimal peer, used only in tests, that serves metadata over BEP 9 and can be told to misbehave | 03 §8. |
| **CI (continuous integration)** | automated procedure | builds and tests every change on a server before it is merged | |
| **GitHub Actions** | CI service | GitHub's; runs *workflows* defined in the repository | This project pins each action to a commit digest. |
| **Dependabot** | GitHub service | proposes dependency updates as changes to review | |

### §10.2 Containers and deployment

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **container image** | archive of a file system | holds a program and everything it needs to run, and is identified by a SHA-256 digest | A **base image** is one that other images build on. *Pinned by digest* means named by that digest, not by a movable name. |
| **distroless image** | container image | has only the program's runtime libraries, with no shell or package manager | This project uses `gcr.io/distroless/cc-debian13:nonroot`, which provides glibc. |
| **container** | process | isolated by the operating system (on Linux, by namespaces and cgroups) so that it sees its own file system, network and process list, and runs from a container image | |
| **container hardening** | set of controls | limits what a container may do: runs as a *non-root* user, with a *read-only root file system*, with all Linux *capabilities* dropped (fine-grained root privileges; not the sense in §8.5), with `no-new-privileges`, and with memory and process-count (*pids*) limits | |
| **volume** | container storage | a directory kept outside a container's image and mounted into it, so its data outlives the container | |
| **secret file** | file | holds a secret (such as a password) and is mounted into a container instead of being passed in an environment variable | |
| **Docker** | container tool set | builds container images and runs containers | A *Docker network* connects chosen containers; a *published port* exposes a container's port on the host. |
| **Docker Compose** | Docker tool | starts several containers together from one YAML file (`docker-compose.yml`) | Each container is a *service* in Compose's terms. |
| **GHCR** | registry of container images | GitHub's | |
| **deployment** | running installation | the system's binaries, configuration and data on particular hosts, run by one operator | |
| **admission** | stage of the crawler role | decides which discovered keys enter the durable queue: it drops keys already in the dedup set, and queues keys seen only in get_peers queries only after sightings from at least 2 distinct IPv4 /24 or IPv6 /48 sources | 03 §13. |
| **hint map** | LRU map | holds, for a short time, peers that announced a key, so a fetch can try them first | 03 §3: 100 000 keys × 8 peers, 5 min. |

### §10.3 Libraries and products named in 01–04

| Term | Genus | Differentia | Note |
|---|---|---|---|
| **tokio** | library | Rust's most widely used asynchronous runtime | |
| **axum** | library | an HTTP server framework for Rust, built on tokio | |
| **askama** | library | a Rust template engine that checks templates when the program is compiled and auto-escapes by default | |
| **sqlx** | library | a Rust client for PostgreSQL and other SQL databases | |
| **serde_json** | library | the standard Rust JSON serializer | |
| **thiserror** | library | derives Rust error types | |
| **tower_governor** | library | rate limiting for axum | 03 §12 forbids its leftmost-`X-Forwarded-For` key extractor. |
| **mainline / librqbit-dht / dht-crawler** | Rust DHT crates | published crates checked in 01 §6; none implements BEP 51 | |
| **Torznab** | HTTP API convention | derived from Newznab; download-automation tools (Jackett, Sonarr, Radarr) use it to query torrent indexes | Deferred (03 §15). |
