# Homebrew formula template for Justless. It builds the tagged source release.
#
# To publish a release to the tap (github.com/rishadbaniya/homebrew-justless):
#   1. Set `url` to the new tag's source archive and `sha256` to its checksum:
#        curl -fsSL https://github.com/rishadbaniya/justless/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256
#   2. Copy this file to Formula/justless.rb in the tap and run
#        brew install --build-from-source ./Formula/justless.rb && brew test justless && brew audit --strict justless
#   3. Commit and push the tap. Users then run `brew install rishadbaniya/justless/justless`.
class Justless < Formula
  desc "Local command output and source context for coding agents"
  homepage "https://github.com/rishadbaniya/justless"
  url "https://github.com/rishadbaniya/justless/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  head "https://github.com/rishadbaniya/justless.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/justless --version")
    (testpath/"lib.rs").write "pub fn answer() -> u32 {\n    42\n}\n"
    assert_match "function answer", shell_output("#{bin}/justless read lib.rs --outline")
  end
end
