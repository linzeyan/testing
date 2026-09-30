// A separate console binary: the GUI exe uses the Windows GUI subsystem, and shells neither
// wait for such programs nor report their exit code, which CI needs.
fn main() {
    std::process::exit(apitool::cli::main());
}
