use super::{read_local, write_csv, Result, MAX_FILE};
use calamine::{DataRef, Reader, Xlsx};
use quick_xml::events::Event;
use serde_json::{json, Value};
use std::io::{self, BufReader, Cursor, Read};
use std::path::Path;
use zip::ZipArchive;

const MAX_EXPANDED: u64 = 64 << 20;
const MAX_MEMBER: u64 = 16 << 20;
const MAX_CELLS: usize = 320_000;

fn address(raw: &str) -> Result<(usize, usize)> {
    let raw = raw.strip_prefix('$').unwrap_or(raw);
    let letters = raw.bytes().take_while(u8::is_ascii_alphabetic).count();
    let digits = raw[letters..].strip_prefix('$').unwrap_or(&raw[letters..]);
    if !(1..=3).contains(&letters)
        || digits.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("expected an A1 cell reference".into());
    }
    let column = raw[..letters].bytes().fold(0usize, |column, byte| {
        column * 26 + usize::from(byte.to_ascii_uppercase() - b'A' + 1)
    });
    let row = digits
        .parse::<usize>()
        .ok()
        .filter(|row| (1..=1_048_576).contains(row));
    if column > 16_384 {
        return Err("column outside XLSX limits".into());
    }
    Ok((row.ok_or("row outside XLSX limits")? - 1, column - 1))
}

struct Area {
    top: usize,
    left: usize,
    height: usize,
    width: usize,
}

impl Area {
    fn parse(raw: &str) -> Result<Self> {
        let (start, end) = raw.split_once(':').unwrap_or((raw, raw));
        let (top, left) = address(start)?;
        let (bottom, right) = address(end)?;
        if bottom < top || right < left {
            return Err("range ends before its start".into());
        }
        let (height, width) = (bottom - top + 1, right - left + 1);
        if height
            .checked_mul(width)
            .is_none_or(|cells| cells > MAX_CELLS)
        {
            return Err("range exceeds 320000 cells".into());
        }
        Ok(Self {
            top,
            left,
            height,
            width,
        })
    }
}

fn check_shared_strings(reader: impl Read) -> Result<()> {
    let mut xml = quick_xml::Reader::from_reader(BufReader::new(reader));
    let mut buffer = Vec::new();
    loop {
        match xml
            .read_event_into(&mut buffer)
            .map_err(|err| err.to_string())?
        {
            Event::Start(tag) | Event::Empty(tag) if tag.local_name().as_ref() == "sst" => {
                if let Some(count) = tag
                    .try_get_attribute("uniqueCount")
                    .map_err(|err| err.to_string())?
                {
                    let count = count.value.parse::<usize>().ok();
                    if count.is_none_or(|value| value > 1_000_000) {
                        return Err("invalid or excessive shared string count".into());
                    }
                }
                return Ok(());
            }
            Event::Eof => return Err("missing shared string table".into()),
            _ => buffer.clear(),
        }
    }
}

fn check_archive(bytes: &[u8]) -> Result<()> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|err| err.to_string())?;
    if archive.len() > 4096
        || archive
            .decompressed_size()
            .is_none_or(|size| size > u128::from(MAX_EXPANDED))
    {
        return Err("workbook exceeds ZIP member count or expanded size limit".into());
    }
    let mut remaining = MAX_EXPANDED;
    for index in 0..archive.len() {
        let mut member = archive.by_index(index).map_err(|err| err.to_string())?;
        let expected = member.size();
        if expected > MAX_MEMBER.min(remaining) {
            return Err("workbook ZIP member exceeds expanded size limit".into());
        }
        let shared = member
            .name()
            .map_err(|err| err.to_string())?
            .rsplit('/')
            .next()
            .is_some_and(|name| name.eq_ignore_ascii_case("sharedStrings.xml"));
        let mut limited = (&mut member).take(expected + 1);
        let actual = if shared {
            let mut data = Vec::new();
            limited
                .read_to_end(&mut data)
                .map_err(|err| err.to_string())?;
            check_shared_strings(data.as_slice())?;
            data.len() as u64
        } else {
            io::copy(&mut limited, &mut io::sink()).map_err(|err| err.to_string())?
        };
        if actual != expected {
            return Err("workbook ZIP member size mismatch".into());
        }
        remaining -= actual;
    }
    Ok(())
}

fn text(value: &DataRef<'_>) -> String {
    match value {
        DataRef::Int(value) => value.to_string(),
        DataRef::Float(value) => value.to_string(),
        DataRef::Bool(value) => value.to_string(),
        DataRef::Error(value) => value.to_string(),
        DataRef::DateTime(value) => value.as_f64().to_string(),
        DataRef::SharedString(value) => (*value).to_owned(),
        DataRef::String(value) | DataRef::DateTimeIso(value) | DataRef::DurationIso(value) => {
            value.clone()
        }
        DataRef::Empty => String::new(),
    }
}

pub(super) fn export(path: &Path, sheet: &str, range: &str, output: &Path) -> Result<Value> {
    if !path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("xlsx"))
    {
        return Err("expected a local .xlsx workbook".into());
    }
    let area = Area::parse(range)?;
    let bytes = read_local(path)?;
    check_archive(&bytes)?;
    let mut workbook = Xlsx::new(Cursor::new(bytes)).map_err(|err| err.to_string())?;
    let mut reader = workbook
        .worksheet_cells_reader(sheet)
        .map_err(|err| err.to_string())?;
    let mut cells = vec![String::new(); area.height * area.width];
    let mut text_size = 0;
    while let Some(cell) = reader.next_cell().map_err(|err| err.to_string())? {
        let (row, column) = cell.get_position();
        let (Some(row), Some(column)) = (
            (row as usize).checked_sub(area.top),
            (column as usize).checked_sub(area.left),
        ) else {
            continue;
        };
        if row < area.height && column < area.width {
            let value = text(cell.get_value());
            text_size += value.len() as u64;
            if text_size > MAX_FILE {
                return Err("selected cell text exceeds 16 MiB".into());
            }
            cells[row * area.width + column] = value;
        }
    }
    write_csv(output, |writer| {
        for row in cells.chunks_exact(area.width) {
            writer.write_record(row).map_err(|err| err.to_string())?;
        }
        Ok(())
    })?;
    Ok(json!({"output":output,"sheet":sheet,"range":range,"rows":area.height,"columns":area.width}))
}

pub(super) fn export_live(
    hwnd: u64,
    book: &str,
    sheet: &str,
    range: &str,
    output: &Path,
) -> Result<Value> {
    let area = Area::parse(range)?;
    #[cfg(windows)]
    {
        let cells = aad_windows::excel::read_range(
            hwnd,
            book,
            sheet,
            [area.top, area.left, area.height, area.width],
        )?;
        write_csv(output, |writer| {
            for row in cells.chunks_exact(area.width) {
                writer.write_record(row).map_err(|e| e.to_string())?;
            }
            Ok(())
        })?;
        Ok(
            json!({"source":"excel_window","window":hwnd,"book":book,"sheet":sheet,"range":range,
            "rows":area.height,"columns":area.width,"output":output}),
        )
    }
    #[cfg(not(windows))]
    {
        let _ = (hwnd, book, sheet, area, output);
        Err("live Excel window export requires Windows desktop Excel".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    #[test]
    fn accepts_only_bounded_rectangles() {
        for invalid in [
            "",
            "B2:A1",
            "0",
            "A0",
            "XFE3",
            "A1048577",
            "A1:B1:C1",
            "A:XFD",
            "A1:Z100000",
            "Sheet1!A1",
        ] {
            assert!(Area::parse(invalid).is_err(), "{invalid}");
        }
        let edge = Area::parse("$xfd$1048576").unwrap();
        assert_eq!(
            (edge.top, edge.left, edge.width, edge.height),
            (1048575, 16383, 1, 1)
        );
    }

    fn package(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in members {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn exports_a_named_sheet_at_an_absolute_offset() {
        let bytes=package(&[
            ("_rels/.rels",br#"<Relationships><Relationship Id="book" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("xl/workbook.xml",br#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Other" sheetId="1" r:id="one"/><sheet name="Bills" sheetId="2" r:id="two"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels",br#"<Relationships><Relationship Id="one" Target="worksheets/one.xml" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"/><Relationship Id="two" Target="worksheets/two.xml" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"/></Relationships>"#),
            ("xl/worksheets/one.xml",b"<worksheet><sheetData/></worksheet>"),
            ("xl/worksheets/two.xml",br#"<worksheet><sheetData><row r="3"><c r="B3" t="inlineStr"><is><t>001,abc</t></is></c><c r="D3"><f>1+2</f><v>3</v></c></row></sheetData></worksheet>"#)
        ]);
        let dir = tempfile::tempdir().unwrap();
        let book = dir.path().join("example.xlsx");
        let output = dir.path().join("result.csv");
        std::fs::write(&book, bytes).unwrap();
        export(&book, "Bills", "B2:D3", &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b",,\r\n\"001,abc\",,3\r\n");
        assert!(export(&book, "Other", "B2:D3", &output).is_err());
    }

    #[test]
    fn bounds_expansion_and_shared_string_allocation() {
        let payload = vec![0; MAX_MEMBER as usize + 1];
        let archive = package(&[("large.bin", &payload)]);
        assert!(archive.len() < 100_000);
        assert!(check_archive(&archive).is_err());
        assert!(check_archive(&package(&[(
            "xl/sharedStrings.xml",
            br#"<sst uniqueCount="18446744073709551615"/>"#
        )]))
        .is_err());
        let mut archive = package(&[("member", &[1; 8192])]);
        let directory = archive
            .windows(4)
            .position(|tag| tag == b"PK\x01\x02")
            .unwrap();
        archive[directory + 24..directory + 28].copy_from_slice(&8u32.to_le_bytes());
        assert!(check_archive(&archive).is_err());
    }
}
