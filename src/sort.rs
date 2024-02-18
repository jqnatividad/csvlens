use crate::csv;

use std::fs::File;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::{self};

use anyhow::Result;
use arrow::array::{Array, ArrayIter};
use arrow::compute::concat;
use arrow::compute::kernels;
use arrow::datatypes::Fields;
use arrow::datatypes::Schema;
use arrow::datatypes::SchemaBuilder;

#[derive(Clone, Debug, PartialEq)]
pub enum SorterStatus {
    Running,
    Finished,
    Error(String),
}

#[derive(Clone, Copy, Debug)]
pub enum SortOrder {
    Ascending,
    Descending,
}

#[derive(Debug)]
pub struct Sorter {
    pub column_index: usize,
    column_name: String,
    order: SortOrder,
    internal: Arc<Mutex<SorterInternalState>>,
}

impl Sorter {
    pub fn new(csv_config: Arc<csv::CsvConfig>, column_index: usize, column_name: String) -> Self {
        let internal = SorterInternalState::init(csv_config, column_index);
        Sorter {
            column_index,
            column_name,
            order: SortOrder::Ascending,
            internal,
        }
    }

    pub fn get_sorted_indices(&self, rows_from: u64, num_rows: u64) -> Option<Vec<u64>> {
        let m_guard = self.internal.lock().unwrap();
        if let Some(sort_result) = &m_guard.sort_result {
            let mut out = vec![];
            for i in rows_from..rows_from + num_rows {
                if let Some(record_index) = sort_result.record_indices.get(i as usize) {
                    out.push(*record_index as u64)
                }
            }
            return Some(out);
        }
        None
    }

    pub fn get_record_order(&self, row_index: u64) -> Option<u64> {
        let m_guard = self.internal.lock().unwrap();
        if let Some(sort_result) = &m_guard.sort_result {
            if let Some(record_order) = sort_result.record_orders.get(row_index as usize) {
                return Some(*record_order as u64);
            }
        }
        None
    }

    pub fn status(&self) -> SorterStatus {
        (self.internal.lock().unwrap()).status.clone()
    }

    pub fn order(&self) -> SortOrder {
        self.order
    }

    pub fn column_name(&self) -> &str {
        self.column_name.as_str()
    }

    pub fn terminate(&self) {
        let mut m = self.internal.lock().unwrap();
        m.terminate();
    }
}

impl Drop for Sorter {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[derive(Debug)]
struct SortResult {
    record_indices: Vec<usize>,
    record_orders: Vec<usize>,
}

#[derive(Debug)]
struct SorterInternalState {
    sort_result: Option<SortResult>,
    status: SorterStatus,
    should_terminate: bool,
    done: bool,
}

impl SorterInternalState {
    pub fn init(
        config: Arc<csv::CsvConfig>,
        column_index: usize,
    ) -> Arc<Mutex<SorterInternalState>> {
        let internal = SorterInternalState {
            sort_result: None,
            status: SorterStatus::Running,
            should_terminate: false,
            done: false,
        };

        let m_state = Arc::new(Mutex::new(internal));

        let _m = m_state.clone();
        let _filename = config.filename().to_owned();
        let _delimiter = config.delimiter();

        let _handle = thread::spawn(move || {
            fn run(
                m: Arc<Mutex<SorterInternalState>>,
                filename: &str,
                _delimiter: u8,
                column_index: usize,
            ) -> Result<SortResult> {
                // Get schema
                let schema = SorterInternalState::infer_schema(filename, _delimiter)?;
                let file = File::open(filename)?;
                let arrow_csv_reader = arrow::csv::ReaderBuilder::new(Arc::new(schema))
                    .with_delimiter(_delimiter)
                    .with_header(true)
                    .with_projection(vec![column_index])
                    .build(file)
                    .unwrap();

                // Parse csv in batches to construct the column
                let mut arrs: Vec<Arc<dyn Array>> = Vec::new();
                for record_batch_result in arrow_csv_reader {
                    let record_batch = record_batch_result?;
                    let arr = record_batch.column(0);
                    arrs.push(arr.clone());
                    if m.lock().unwrap().should_terminate {
                        return Err(anyhow::anyhow!("Terminated"));
                    }
                }
                let ref_arrs = arrs
                    .iter()
                    .map(|arr| arr.as_ref())
                    .collect::<Vec<&dyn Array>>();
                let combined_arr = concat(&ref_arrs).unwrap();

                // Sort
                let sorted_indices =
                    kernels::sort::sort_to_indices(combined_arr.as_ref(), None, None).unwrap();

                // Construct the result. Maybe this can be kept as arrow Arrays?
                let mut sorted_record_indices: Vec<usize> = vec![];
                let mut record_orders: Vec<usize> = vec![0; sorted_indices.len()];
                for (record_order, sorted_record_index) in
                    ArrayIter::new(&sorted_indices).flatten().enumerate()
                {
                    sorted_record_indices.push(sorted_record_index as usize);
                    record_orders[sorted_record_index as usize] = record_order;
                }
                let sort_result = SortResult {
                    record_indices: sorted_record_indices,
                    record_orders,
                };
                Ok(sort_result)
            }

            let sort_result = run(_m.clone(), _filename.as_str(), _delimiter, column_index);

            let mut m = _m.lock().unwrap();
            if let Ok(sort_result) = sort_result {
                m.sort_result = Some(sort_result);
                m.status = SorterStatus::Finished;
            } else {
                m.status = SorterStatus::Error(sort_result.err().unwrap().to_string());
            }
            m.done = true;
        });

        m_state
    }

    fn infer_schema(filename: &str, delimiter: u8) -> Result<Schema> {
        let schema = arrow::csv::infer_schema_from_files(
            &[filename.to_string()],
            delimiter,
            Some(1000),
            true,
        )?;

        // Convert integer fields to float64 to be more permissive
        let mut updated_fields = vec![];
        for field in schema.fields() {
            if field.data_type().is_integer() {
                let new_field = field
                    .as_ref()
                    .clone()
                    .with_data_type(arrow::datatypes::DataType::Float64);
                updated_fields.push(new_field);
            } else {
                updated_fields.push(field.as_ref().clone());
            }
        }
        let updated_fields = Fields::from(updated_fields);

        Ok(SchemaBuilder::from(updated_fields).finish())
    }

    fn terminate(&mut self) {
        self.should_terminate = true;
    }
}

mod tests {

    use super::*;
    use core::time;

    impl Sorter {
        fn wait_internal(&self) {
            loop {
                if self.internal.lock().unwrap().done {
                    break;
                }
                thread::sleep(time::Duration::from_millis(100));
            }
        }
    }

    #[test]
    fn test_simple() {
        let config = Arc::new(csv::CsvConfig::new("tests/data/simple.csv", b','));
        let s = Sorter::new(config, 0, "A1".to_string());
        s.wait_internal();
        let rows = s.get_sorted_indices(0, 5).unwrap();
        let expected = vec![0, 9, 99, 999, 1000];
        assert_eq!(rows, expected);
    }
}