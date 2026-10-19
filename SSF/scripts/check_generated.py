import hashlib
import pathlib
import subprocess

files = list(pathlib.Path("api/ssf/v1").glob("*.pb.go"))
before = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in files}
subprocess.run(["go", "run", "./tools/generate"], check=True)
after = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in pathlib.Path("api/ssf/v1").glob("*.pb.go")}
if before != after:
    raise SystemExit("generated protobuf files were stale; review generated changes")
print("protobuf generation is reproducible")
