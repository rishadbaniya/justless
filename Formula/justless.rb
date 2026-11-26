# Build-from-source Homebrew formula template for Justless.
# Builds strictly from source via Cargo and never downloads prebuilt binaries.
# Update `url` and add `sha256` when tagging a release, or install HEAD directly:
#   brew install --HEAD Formula/justless.rb
class Justless < Formula
  desc "Compact command output and local code graph for coding agents"
  homepage "https://github.com/rishadbaniya/justless"
  url "https://github.com/rishadbaniya/justless/archive/refs/tags/v0.1.0.tar.gz"
  # sha256 "REPLACE_WITH_TAG_TARBALL_SHA256"
  license "Apache-2.0"
  head "https://github.com/rishadbaniya/justless.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args
  end

  test do
    assert_match "justless", shell_output("#{bin}/justless --version")
  end
end
