# filler-remove

Regex-based filler word and stutter removal from transcribed speech.

## Usage

### Built-in patterns

```rust
use filler_remove::remove_filler_words;

let cleaned = remove_filler_words("um, I uh went to the store", &[]);
assert_eq!(cleaned, "I went to the store");

// Russian hesitations are built in too.
let cleaned = remove_filler_words("Э-э-э, я пошёл", &[]);
assert_eq!(cleaned, "Я пошёл");
```

### Custom filler words for other languages

Built-in patterns cover English fillers and Russian hesitation sounds
("э-э-э", "эм", "ммм"). For other words or languages, supply your own custom
filler words (they replace the built-ins):

```rust
use filler_remove::remove_filler_words;

let custom = vec!["ну".to_string(), "типа".to_string()];
let cleaned = remove_filler_words("ну типа я пошёл", &custom);
assert_eq!(cleaned, "я пошёл");
```

### Stutter removal

Stutters (consecutive repeated words) are always removed regardless of the
filler list:

```rust
use filler_remove::remove_filler_words;

let cleaned = remove_filler_words("I I I went to the store", &[]);
assert_eq!(cleaned, "I went to the store");
```

### Reusing a filter on hot paths

`remove_filler_words` recompiles its regexes on every call. For per-chunk or
per-recording filtering, build a `FillerFilter` once and reuse it. The
constructor returns `Result<_, regex::Error>` so misconfigured custom words
surface as real errors instead of being silently dropped:

```rust
use filler_remove::FillerFilter;

let filter = FillerFilter::new(&["ну", "типа"]).unwrap();
for chunk in ["ну я пошёл", "типа домой"] {
    println!("{}", filter.apply(chunk));
}
```

## Installation

```sh
cargo add filler-remove
```

## License

MIT
