## eruption-audio-proxy - Audio proxy daemon for the Eruption Linux user-mode driver

A daemon that delivers an audio stream to the `Eruption` daemon where it can be processed, e.g. for consumption by audio visualizer plugins. Additionally the `eruption-audio-proxy` can play back sound effects, triggered by `Eruption`.

### eruption-audio-proxy

```shell
eruption-audio-proxy 0.0.2

X3n0m0rph59 <x3n0m0rph59@gmail.com>

Audio proxy daemon for the Eruption Linux user-mode driver

USAGE:
    eruption-audio-proxy [FLAGS] [OPTIONS] <SUBCOMMAND>

FLAGS:
    -h, --help       Print help information
    -v, --verbose    Verbose mode (-v, -vv, -vvv, etc.)
    -V, --version    Print version information

OPTIONS:
    -c, --config <CONFIG>    Sets the configuration file to use

SUBCOMMANDS:
    completions    Generate shell completions
    daemon         Run in background
    help           Print this message or the help of the given subcommand(s)
```