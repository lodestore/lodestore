use std::collections::HashMap;
use std::path::PathBuf;
use std::process::exit;

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(fixtures), Some(corpus), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: seed-corpus <fixtures-dir> <corpus-dir>");
        exit(2);
    };
    let corpus = PathBuf::from(corpus);
    let seeds = match lodestore_fuzzkit::seeds::try_seeds(&PathBuf::from(fixtures)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("seed-corpus: {e}");
            exit(2);
        }
    };
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for (target, bytes) in seeds {
        let n = counts.entry(target).or_default();
        let dir = corpus.join(target);
        let written = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(dir.join(format!("seed-{n}")), bytes));
        if let Err(e) = written {
            eprintln!("seed-corpus: {}: {e}", dir.display());
            exit(2);
        }
        *n += 1;
    }
}
