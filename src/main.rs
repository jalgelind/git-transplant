use clap::{Args, Parser, Subcommand, ValueEnum};

// cli tool to move changes around
// "git transplant"? "git graft"??

// Various tools I want:
//
// A) "Collapse $chunk and related lines in parent commits to $target"
//
// B) Move file to $target; remove file from all parents of $target
//
// C) Apply fixup commit to $target. Abort if it conflicts with parents.
//
// D): Apply fixup commit to $target, take related changes from parent commits.
//
//
// General approach is to create a new branch and gradually commit amended
// commits there; taking hunks from commits in the target branch. Can this be
// done on just the index?

/*
    libs
    ----
    git2        git stuff
    clap        arg parsing
    anyhow      error handling
    inquire     interactive prompting
*/

#[derive(Debug, Parser)] // requires `derive` feature
#[command(name = "git-transplant", version = "0.1", author = "Johannes")]
struct Opts {
    #[clap(short = 's', long = "something", default_value = "")]
    something: String,

    target: Option<String>,
}

fn main() {
    let opts = Opts::parse();
    println!("{:#?}", opts);
}
