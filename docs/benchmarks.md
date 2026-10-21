# Benchmarks

Run them with one command from the repository root:

```bash
cargo run --release -p groovy-core --example bench
```

The defaults are a 1 GB file and 10,000 small files. For the full sizes in the issue:

```bash
cargo run --release -p groovy-core --example bench -- --large-mb 10240 --small-files 100000
```

The benchmark writes its data under `target/bench` and removes it at the end. It prints a table, so the numbers can be copied here. The large file is written in 1 MB chunks, so the run does not need that much memory. A full run needs about 40 GB of free disk.

## What is measured

- **Import one large file**: the file is hashed and copied into a blob store, as the Files screen does when you send.
- **Import many small files**: the same, for a folder of small files, in folders of 100.
- **Make a share**: a share of the large file, as the share control makes it.
- **Transfer over loopback**: a share is received by a second node in the same process, from start to the file being saved. It runs over the real protocol, on loopback, with no relay.

## Results

Machine: Intel Core i5-8365U (4 cores, 8 threads), 7 GB RAM, NVMe SSD, Linux 7.2. Built with `--release`.

### Full sizes

| Measurement | Size | Time | Throughput |
|---|---|---|---|
| Import one large file | 10240 MB | 12.6 s | 854 MB/s |
| Import 100000 small files | 1024 B each | 19.9 s | 5020 files/s, 5.1 MB/s |
| Make a share of the large file | 10240 MB | 12.7 s | 844 MB/s |
| Transfer over loopback | 10240 MB | 155.3 s | 69 MB/s |

### Default sizes

| Measurement | Size | Time | Throughput |
|---|---|---|---|
| Import one large file | 1024 MB | 1.3 s | 846 MB/s |
| Import 10000 small files | 1024 B each | 1.6 s | 6288 files/s, 6.4 MB/s |
| Make a share of the large file | 1024 MB | 1.6 s | 664 MB/s |
| Transfer over loopback | 1024 MB | 11.8 s | 91 MB/s |

## Reading the numbers

- Throughput is in decimal megabytes per second, and the time includes writing the data to disk.
- The loopback transfer is slower than the import, because the received bytes are checked as they arrive and then written to the store and the downloads folder. The full-size run was slower per byte than the default run, probably because it also wrote far more data to the same disk.
- Small files cost more per byte, because each one is a separate import. The rate is limited by files per second, not by bytes.
- These are one machine and one run each, so expect some variation from run to run. Compare numbers from the same machine.

## Resume after an interruption

`cargo run --release -p groovy-core --example resume_check` shares a 5 GiB file over loopback, pauses the receive once a quarter of it has arrived, resumes it, and checks that the resume fetched only what was missing and that the file is intact.

Pause result on the same machine: paused with 1,285 MiB of 5,120 MiB received. The resume took 61 s and fetched 3,834 MiB, which is what was still missing (3,835 MiB). The file arrived intact.

The same check can interrupt by stopping the node and starting it again, which is what closing the app does: `--interrupt restart`. The node stops the receive and queues it, and the next start picks it up from what it has, without anything asking for it.

Result with a restart on the same machine: stopped with 1,283 MiB of 5,120 MiB received. After the restart the receive fetched 3,837 MiB, which is what was missing. The file arrived intact.

### A crash (SIGKILL)

`scripts/crash-check.sh` shares a file between two data directories, kills the receiver with SIGKILL six seconds in, resumes it in a new process with `groovy-cli resume`, and checks the saved file against the source. Use the release build of the CLI for large sizes: a debug build is many times slower.

Result at 5 GiB, with the release CLI: the receive had 470,941,782 bytes when it was killed. The resume fetched 4,897,439,744 bytes, which is what was missing, and the file matches the source.

An earlier version of this section said the 5 GiB case failed with `encode error` because of the unclean stop. That was wrong. The cause was garbage collection: a transfer that took longer than the collection interval (60 s) had its partly downloaded files removed, and failed when it was saved. A plain 2 GiB transfer with no kill hit it too. A debug build is slow enough to be affected, and a release build usually finishes first. A transfer now takes out its tag when it starts, and protects every file in it before any data arrives.

The check does not cover a dropped network connection.

## Scrolling a long conversation

`cargo run -p groovy-core --example fill_conversation -- <data dir> <own endpoint id> 10000` fills a database with a 10,000-message conversation. The chat loads the newest 100 messages, and the page before the oldest one shown each time the list reaches the top.

Measured on the release build (Intel UHD 620 laptop, X11), scrolling up continuously for about 30 seconds, with the time from the start of a frame's render to the end of its paint:

| Messages loaded | Frames | Median | 95th percentile | Slowest |
| --- | --- | --- | --- | --- |
| 0 to 999 | 218 | 16.5 ms | 19.4 ms | 20.9 ms |
| 1,000 to 2,999 | 444 | 16.4 ms | 19.1 ms | 48.5 ms |
| 3,000 to 5,999 | 259 | 16.3 ms | 18.9 ms | 22.4 ms |

The cost of a frame does not grow with the conversation: the list draws only the messages on screen. An idle frame of the same screen, with no conversation open, costs about 6 to 13 ms, so most of a frame is the screen around the chat. A debug build is about five times slower.
