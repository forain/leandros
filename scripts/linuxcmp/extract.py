import sys, os, gzip, tarfile, io
out = sys.argv[1]; os.makedirs(out, exist_ok=True)
for f in sys.argv[2:]:
    data = gzip.decompress(open(f, "rb").read())
    t = tarfile.open(fileobj=io.BytesIO(data), ignore_zeros=True)
    ms = [m for m in t.getmembers() if not m.name.startswith(".")]
    t.extractall(out, members=ms, filter="data")
