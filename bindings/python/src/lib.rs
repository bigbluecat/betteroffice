//! PyO3 bindings over the `betteroffice-xlsx` facade.
//!
//! The Rust surface here is deliberately flat: every method takes a resolved
//! sheet plus an A1 address. The ergonomic `Sheet` proxy lives in the Python
//! layer, which keeps borrow lifetimes out of the boundary.

use std::fs;
use std::path::PathBuf;

use pyo3::create_exception;
use pyo3::exceptions::{PyException, PyIndexError, PyKeyError, PyOSError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;

use betteroffice_xlsx::{
    CalculationOptions, CellRange, CellRef, CellValue, Error as CoreError, RenderOptions, SheetId,
    Workbook as CoreWorkbook,
};

create_exception!(
    _betteroffice_xlsx,
    XlsxError,
    PyException,
    "Base class for every error raised by the engine."
);
create_exception!(
    _betteroffice_xlsx,
    ParseError,
    XlsxError,
    "The workbook could not be read."
);
create_exception!(
    _betteroffice_xlsx,
    RangeError,
    XlsxError,
    "A sheet, cell, or range was out of bounds or too large."
);
create_exception!(
    _betteroffice_xlsx,
    RenderError,
    XlsxError,
    "Rendering failed or exceeded a size limit."
);

fn map_error(error: CoreError) -> PyErr {
    let message = error.to_string();
    match error {
        CoreError::Package(_)
        | CoreError::Spreadsheet(_)
        | CoreError::DuplicatePart(_)
        | CoreError::NoSheets => ParseError::new_err(message),
        CoreError::SheetOutOfRange(_)
        | CoreError::CellOutOfRange(_)
        | CoreError::RangeTooLarge { .. }
        | CoreError::DisplayTooLarge { .. }
        | CoreError::InvalidViewport => RangeError::new_err(message),
        CoreError::InvalidScale(_)
        | CoreError::RenderTooLarge { .. }
        | CoreError::RenderAreaTooLarge { .. }
        | CoreError::Raster(_) => RenderError::new_err(message),
        _ => XlsxError::new_err(message),
    }
}

fn parse_cell(address: &str) -> PyResult<CellRef> {
    CellRef::parse_a1(address)
        .map_err(|error| RangeError::new_err(format!("invalid cell {address:?}: {error}")))
}

fn parse_range(address: &str) -> PyResult<CellRange> {
    CellRange::parse_a1(address)
        .map_err(|error| RangeError::new_err(format!("invalid range {address:?}: {error}")))
}

/// An Excel error value such as `#DIV/0!`, distinct from a cell holding that
/// text.
#[pyclass(module = "betteroffice_xlsx", name = "CellError", frozen)]
pub struct PyCellError {
    #[pyo3(get)]
    code: String,
}

#[pymethods]
impl PyCellError {
    fn __str__(&self) -> &str {
        &self.code
    }

    fn __repr__(&self) -> String {
        format!("CellError({:?})", self.code)
    }

    fn __eq__(&self, other: &Bound<'_, PyAny>) -> bool {
        if let Ok(other) = other.extract::<PyRef<'_, Self>>() {
            return self.code == other.code;
        }
        other
            .extract::<String>()
            .is_ok_and(|text| text == self.code)
    }

    /// Delegates to the code's `str` hash. `__eq__` accepts a plain `str`, so
    /// the hashes have to agree or dict and set lookups break.
    fn __hash__(&self, py: Python<'_>) -> PyResult<isize> {
        self.code.as_str().into_pyobject(py)?.hash()
    }
}

/// A rendered sheet image.
#[pyclass(module = "betteroffice_xlsx", name = "Png", frozen)]
pub struct PyPng {
    data: Vec<u8>,
    #[pyo3(get)]
    width: u32,
    #[pyo3(get)]
    height: u32,
}

#[pymethods]
impl PyPng {
    #[getter]
    fn bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.data)
    }

    fn write(&self, path: PathBuf) -> PyResult<()> {
        fs::write(path, &self.data).map_err(|error| PyOSError::new_err(error.to_string()))
    }

    fn __len__(&self) -> usize {
        self.data.len()
    }

    fn __repr__(&self) -> String {
        format!(
            "Png(width={}, height={}, bytes={})",
            self.width,
            self.height,
            self.data.len()
        )
    }
}

/// The result of a recalculation pass.
#[pyclass(module = "betteroffice_xlsx", name = "Calculation", frozen)]
pub struct PyCalculation {
    #[pyo3(get)]
    changed: usize,
    #[pyo3(get)]
    cycles: usize,
    #[pyo3(get)]
    limited: usize,
}

#[pymethods]
impl PyCalculation {
    fn __repr__(&self) -> String {
        format!(
            "Calculation(changed={}, cycles={}, limited={})",
            self.changed, self.cycles, self.limited
        )
    }
}

#[pyclass(module = "betteroffice_xlsx", name = "Workbook")]
pub struct PyWorkbook {
    inner: CoreWorkbook,
}

impl PyWorkbook {
    fn resolve_sheet(&self, key: &Bound<'_, PyAny>) -> PyResult<SheetId> {
        if let Ok(name) = key.extract::<String>() {
            return self
                .inner
                .sheet_id(&name)
                .ok_or_else(|| PyKeyError::new_err(format!("no sheet named {name:?}")));
        }
        if let Ok(index) = key.extract::<usize>() {
            let count = self.inner.sheet_count();
            if index >= count {
                return Err(PyIndexError::new_err(format!(
                    "sheet index {index} out of range for {count} sheet(s)"
                )));
            }
            return Ok(SheetId(index as u32));
        }
        Err(PyTypeError::new_err(
            "sheet must be a name (str) or an index (int)",
        ))
    }
}

#[pymethods]
impl PyWorkbook {
    /// Open a workbook from bytes without recalculating.
    #[staticmethod]
    fn open(data: &[u8]) -> PyResult<Self> {
        CoreWorkbook::open(data)
            .map(|inner| Self { inner })
            .map_err(map_error)
    }

    /// Open a workbook from a filesystem path.
    #[staticmethod]
    fn open_path(path: PathBuf) -> PyResult<Self> {
        let data = fs::read(&path).map_err(|error| {
            PyOSError::new_err(format!("could not read {}: {error}", path.display()))
        })?;
        Self::open(&data)
    }

    /// Open a workbook and recalculate every formula up front.
    #[staticmethod]
    #[pyo3(signature = (data, *, now_serial = None))]
    fn open_recalculated(data: &[u8], now_serial: Option<f64>) -> PyResult<Self> {
        CoreWorkbook::open_recalculated(data, CalculationOptions { now_serial })
            .map(|inner| Self { inner })
            .map_err(map_error)
    }

    /// Recalculate every formula in the workbook.
    #[pyo3(signature = (*, now_serial = None))]
    fn recalculate(&mut self, now_serial: Option<f64>) -> PyCalculation {
        let result = self
            .inner
            .recalculate_all(CalculationOptions { now_serial });
        PyCalculation {
            changed: result.changed.len(),
            cycles: result.cycle_cells.len(),
            limited: result.limited_cells.len(),
        }
    }

    #[getter]
    fn sheet_count(&self) -> usize {
        self.inner.sheet_count()
    }

    /// Resolve a sheet name or index to its positional index.
    fn sheet_index(&self, sheet: &Bound<'_, PyAny>) -> PyResult<usize> {
        self.resolve_sheet(sheet).map(|sheet| sheet.0 as usize)
    }

    #[getter]
    fn sheet_names(&self) -> PyResult<Vec<String>> {
        (0..self.inner.sheet_count())
            .map(|index| {
                self.inner
                    .sheet(SheetId(index as u32))
                    .map(|sheet| sheet.name.clone())
                    .map_err(map_error)
            })
            .collect()
    }

    /// The calculated value of a cell.
    fn value(
        &self,
        py: Python<'_>,
        sheet: &Bound<'_, PyAny>,
        address: &str,
    ) -> PyResult<Py<PyAny>> {
        let sheet = self.resolve_sheet(sheet)?;
        let cell = parse_cell(address)?;
        let sheet = self.inner.sheet(sheet).map_err(map_error)?;
        let Some(found) = sheet.cell(cell) else {
            return Ok(py.None());
        };
        match &found.value {
            CellValue::Empty => Ok(py.None()),
            CellValue::Number { value } => Ok(value.into_pyobject(py)?.unbind().into_any()),
            CellValue::Text { value } => Ok(value.into_pyobject(py)?.unbind().into_any()),
            CellValue::Bool { value } => Ok(value
                .into_pyobject(py)
                .map(|bound| bound.to_owned())?
                .unbind()
                .into_any()),
            CellValue::Error { value } => Ok(Py::new(
                py,
                PyCellError {
                    code: value.as_str().to_string(),
                },
            )?
            .into_any()),
        }
    }

    /// The source formula of a cell, without the leading `=`, or `None`.
    fn formula(&self, sheet: &Bound<'_, PyAny>, address: &str) -> PyResult<Option<String>> {
        let sheet = self.resolve_sheet(sheet)?;
        let cell = parse_cell(address)?;
        let sheet = self.inner.sheet(sheet).map_err(map_error)?;
        Ok(sheet.cell(cell).and_then(|found| found.formula.clone()))
    }

    /// Set a cell from what a user would type. A leading `=` makes it a
    /// formula. Dependents recalculate. Returns whether anything changed.
    #[pyo3(signature = (sheet, address, value, *, now_serial = None))]
    fn set(
        &mut self,
        sheet: &Bound<'_, PyAny>,
        address: &str,
        value: &str,
        now_serial: Option<f64>,
    ) -> PyResult<bool> {
        let sheet = self.resolve_sheet(sheet)?;
        let cell = parse_cell(address)?;
        self.inner
            .edit_cell(sheet, cell, value, CalculationOptions { now_serial })
            .map(|result| result.applied)
            .map_err(map_error)
    }

    /// Render a sheet to PNG.
    #[pyo3(signature = (sheet, *, scale = 1.0, range = None, max_width = None, max_height = None))]
    fn render_png(
        &self,
        sheet: &Bound<'_, PyAny>,
        scale: f32,
        range: Option<&str>,
        max_width: Option<u32>,
        max_height: Option<u32>,
    ) -> PyResult<PyPng> {
        let sheet = self.resolve_sheet(sheet)?;
        let range = range.map(parse_range).transpose()?;
        let rendered = self
            .inner
            .render_sheet(
                sheet,
                &RenderOptions {
                    range,
                    scale,
                    max_width,
                    max_height,
                },
            )
            .map_err(map_error)?;
        Ok(PyPng {
            data: rendered.bytes,
            width: rendered.width,
            height: rendered.height,
        })
    }

    /// Serialize the workbook back to XLSX bytes.
    fn save<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = self.inner.save().map_err(map_error)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Serialize the workbook to a filesystem path.
    fn save_path(&self, path: PathBuf) -> PyResult<()> {
        let bytes = self.inner.save().map_err(map_error)?;
        fs::write(&path, bytes).map_err(|error| {
            PyOSError::new_err(format!("could not write {}: {error}", path.display()))
        })
    }

    fn __len__(&self) -> usize {
        self.inner.sheet_count()
    }

    fn __repr__(&self) -> String {
        format!("Workbook(sheets={})", self.inner.sheet_count())
    }
}

#[pymodule]
fn _betteroffice_xlsx(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add_class::<PyWorkbook>()?;
    module.add_class::<PyCellError>()?;
    module.add_class::<PyPng>()?;
    module.add_class::<PyCalculation>()?;
    module.add("XlsxError", py.get_type::<XlsxError>())?;
    module.add("ParseError", py.get_type::<ParseError>())?;
    module.add("RangeError", py.get_type::<RangeError>())?;
    module.add("RenderError", py.get_type::<RenderError>())?;
    Ok(())
}
