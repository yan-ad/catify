#!/usr/bin/env python3
import argparse
p=argparse.ArgumentParser(); p.add_argument('--version',required=True); p.add_argument('--url',required=True); p.add_argument('--sha256',required=True); p.add_argument('--output',default='dist/homebrew/catify.rb'); a=p.parse_args()
from pathlib import Path
out=Path(a.output); out.parent.mkdir(parents=True,exist_ok=True)
out.write_text(f'''class Catify < Formula\n  desc "Memory-efficient Shopify CLI alternative"\n  homepage "https://github.com/yan-ad/catify"\n  version "{a.version}"\n  on_macos do\n    if Hardware::CPU.arm?\n      url "{a.url}/cfy-v{a.version}-aarch64-apple-darwin.tar.gz"\n      sha256 "{a.sha256}"\n    end\n  end\n  def install\n    bin.install "cfy"\n  end\n  test do\n    assert_match "cfy", shell_output("#{{bin}}/cfy version")\n  end\nend\n''')
print(out)
