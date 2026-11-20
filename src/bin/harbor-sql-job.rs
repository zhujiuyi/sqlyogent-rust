fn main() {
    std::process::exit(harbor_sql::jobs::cli(std::env::args_os().skip(1)));
}
