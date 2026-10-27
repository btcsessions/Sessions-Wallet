# MarmotKit smoke test

Two accounts, one group, wallet messages both ways, chat, and a runtime restart
with the key held only in memory: the same calls the Android plugin makes.

```sh
# Python bindings from the host library built by scripts/build-marmotkit.sh
target/mdk/release/uniffi-bindgen generate --library target/mdk/release/libmarmot_uniffi.so \
  --language python --out-dir /tmp/marmot-py   # run from the MDK checkout
ln -s "$PWD/target/mdk/release/libmarmot_uniffi.so" /tmp/marmot-py/
pip install websockets
python3 scripts/marmot-test/relay.py 7771 & python3 scripts/marmot-test/relay.py 7772 &
python3 -I scripts/marmot-test/two_members.py /tmp/marmot-py
```
