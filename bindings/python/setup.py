"""Build the ctypes runtime on the consumer host for source distributions."""
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tempfile

from setuptools import setup
from setuptools.command.build_py import build_py
from wheel.bdist_wheel import bdist_wheel


class BuildWithNative(build_py):
    def run(self):
        root = Path(__file__).resolve().parent
        library = root / "skarve/_native/libraster_engine.so"
        binary = root / "skarve/_native/skarve"
        if not library.is_file() or not binary.is_file():
            if platform.system() != "Linux" or platform.machine() != "x86_64":
                raise RuntimeError("Skarve source installation currently supports Linux x86-64 only")
            sources = list((root / "native_source").glob("skarve-*/Cargo.toml"))
            if len(sources) != 1:
                raise RuntimeError("Skarve source package is incomplete: missing Rust crate")
            cargo = shutil.which("cargo") or str(Path.home() / ".cargo/bin/cargo")
            if not Path(cargo).is_file() and not shutil.which(cargo):
                raise RuntimeError("Skarve needs Rust 1.98.1 to build from source; install rustup first")
            if not shutil.which("gdal-config") or not shutil.which("pkg-config"):
                raise RuntimeError("Skarve needs GDAL and libdeflate development packages (libgdal-dev libdeflate-dev) and pkg-config")
            if subprocess.run(["gdal-config", "--version"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
                raise RuntimeError("Skarve needs working GDAL development files (libgdal-dev)")
            if subprocess.run(["pkg-config", "--modversion", "libdeflate"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
                raise RuntimeError("Skarve needs libdeflate development files (libdeflate-dev)")
            with tempfile.TemporaryDirectory(prefix="skarve-python-native-") as temporary:
                env = os.environ.copy()
                env["CARGO_TARGET_DIR"] = temporary
                env["CARGO_BUILD_JOBS"] = "2"
                env["RUSTFLAGS"] = f"--remap-path-prefix={sources[0].parent}=skarve"
                subprocess.run([cargo, "build", "--locked", "--release", "-j", "2"],
                               cwd=sources[0].parent, env=env, check=True)
                built = Path(temporary) / "release/libraster_engine.so"
                built_binary = Path(temporary) / "release/raster-engine"
                if not built.is_file() or not built_binary.is_file():
                    raise RuntimeError("Cargo did not produce the Skarve native library and CLI")
                library.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(built, library)
                shutil.copy2(built_binary, binary)
        super().run()


class PlatformWheel(bdist_wheel):
    def finalize_options(self):
        super().finalize_options()
        self.root_is_pure = False

    def get_tag(self):
        return "py3", "none", "linux_x86_64"


setup(cmdclass={"bdist_wheel": PlatformWheel, "build_py": BuildWithNative})
