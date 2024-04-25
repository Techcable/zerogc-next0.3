# copygc
A safe garbage-collector API for rust.

The name `copygc` is a misnomer, because it doesn't always copy.
However the API is based around the illusion of copying.

It's as if types are being copied from ones with the old gc lifetime `'gc` into types with a new gc lifetime `'newgc`.

This is a potential replacement for the [zerogc](https://github.com/DuckLogic/zerogc) API, which is currently known to be unsound.
