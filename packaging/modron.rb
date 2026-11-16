# Homebrew formula — source build. Bottles follow the first tagged release.
class Modron < Formula
  desc "Token-efficient Rust AI coding agent harness"
  homepage "https://github.com/jbeck018/modron"
  url "https://github.com/jbeck018/modron.git",
      tag:      "v0.1.1",
      revision: "adecea0a9b066c3962ea85008f1ac58150bdb796"
  version "0.1.1"
  license "MIT"

  depends_on "rust" => :build

  def install
    # --root #{prefix}: without it cargo installs into $CARGO_HOME/bin and
    # the formula's bin/modron (and the test block) never exists.
    system "cargo", "install", "--locked", "--root", prefix, "--path", "crates/modron-cli"
    # The terminal app (`modron app`) lives in app/ and needs a tag that has
    # it; older tags have no app/ directory and install modron alone.
    if (buildpath/"app/Cargo.toml").exist?
      system "cargo", "install", "--locked", "--root", prefix, "--path", "app"
    end
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/modron --version")
    system "#{bin}/modron", "smoke", "--provider", "mock"
  end
end
