use uuid::Uuid;

const TS_LEN: usize = 22;
const UUID_LEN: usize = 36;

/// A parsed `dotted_order`: one segment per ancestor, root first, ending with the run itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DottedOrder {
    pub ids: Vec<Uuid>,
}

impl DottedOrder {
    /// Segments are `<YYYYMMDDTHHMMSSffffffZ><UUID>` joined by `.`.
    pub fn parse(s: &str) -> Result<DottedOrder, String> {
        if s.is_empty() {
            return Err("empty".into());
        }
        let ids = s
            .split('.')
            .enumerate()
            .map(|(i, seg)| parse_segment(seg).map_err(|e| format!("segment {i}: {e}")))
            .collect::<Result<Vec<_>, _>>()?;
        if ids.len() > u16::MAX as usize {
            return Err("too deep".into());
        }
        Ok(DottedOrder { ids })
    }

    pub fn depth(&self) -> u16 {
        (self.ids.len() - 1) as u16
    }

    pub fn trace_id(&self) -> Uuid {
        self.ids[0]
    }

    pub fn run_id(&self) -> Uuid {
        self.ids[self.ids.len() - 1]
    }

    pub fn parent_id(&self) -> Option<Uuid> {
        (self.ids.len() >= 2).then(|| self.ids[self.ids.len() - 2])
    }
}

fn parse_segment(seg: &str) -> Result<Uuid, String> {
    if seg.len() != TS_LEN + UUID_LEN || !seg.is_ascii() {
        return Err(format!("expected {} chars, got {}", TS_LEN + UUID_LEN, seg.len()));
    }
    let (ts, id) = seg.split_at(TS_LEN);
    let b = ts.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if !(digits(0..8) && b[8] == b'T' && digits(9..21) && b[21] == b'Z') {
        return Err(format!("bad timestamp {ts:?}, expected YYYYMMDDTHHMMSSffffffZ"));
    }
    Uuid::try_parse(id).map_err(|_| format!("bad uuid {id:?}"))
}
