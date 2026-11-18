# Local ML dependency licences

The ML image is built from the locked `ml/uv.lock`; runtime code does not
download models or call hosted inference services. The direct runtime
dependencies and their upstream-declared licences are:

| Component | Purpose | Licence |
| --- | --- | --- |
| [psycopg](https://www.psycopg.org/psycopg3/docs/) | PostgreSQL jobs and writes | LGPL-3.0-only |
| [pypdf](https://github.com/py-pdf/pypdf/blob/main/LICENSE) | PDF text extraction | BSD-3-Clause |
| [pypdfium2](https://pypdfium2.readthedocs.io/en/stable/readme.html#licensing) | Page rendering for OCR | Apache-2.0 OR BSD-3-Clause, plus bundled PDFium dependency licences |
| [Apache Arrow / PyArrow](https://github.com/apache/arrow/blob/main/LICENSE.txt) | Parquet exports | Apache-2.0 |
| [NumPy](https://github.com/numpy/numpy/blob/main/LICENSE.txt) | Array operations | BSD-3-Clause, with bundled-component notices |
| [Pillow](https://github.com/python-pillow/Pillow/blob/main/LICENSE) | OCR image conversion | MIT-CMU |
| [pytesseract](https://github.com/madmaze/pytesseract/blob/master/LICENSE) | Tesseract subprocess adapter | Apache-2.0 |
| [scikit-learn](https://github.com/scikit-learn/scikit-learn/blob/main/COPYING) | Hashing vectors and TF-IDF | BSD-3-Clause |
| [Zarr Python](https://github.com/zarr-developers/zarr-python/blob/main/LICENSE.txt) | Local embedding arrays | MIT |
| [Tesseract](https://github.com/tesseract-ocr/tesseract/blob/main/LICENSE) | Local OCR engine | Apache-2.0 |

`pypdfium2` wheels include PDFium and third-party licence material. Image
redistribution must preserve that installed material, the Python distribution
licence files, and Debian's Tesseract notices. An SBOM/licence scan should be a
release gate whenever the lockfile or base image changes.

PyMuPDF is intentionally not used: upstream offers it under AGPL or a
commercial licence, which would impose a different distribution decision on
this service.
