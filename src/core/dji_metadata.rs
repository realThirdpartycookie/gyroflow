// SPDX-License-Identifier: GPL-3.0-or-later
//! Preserve native DJI sample descriptions, which FFmpeg cannot mux correctly.
//! Full-length, normal-speed, non-fragmented MP4/MOV exports only. Unknown
//! protobuf fields are copied verbatim instead of round-tripping through prost.
use std::io::{self, Read, Seek, SeekFrom, Write};

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u32_at(data: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_be_bytes(
        data.get(offset..offset + 4)
            .ok_or_else(|| invalid("Truncated MP4"))?
            .try_into()
            .unwrap(),
    ))
}
fn u64_at(data: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_be_bytes(
        data.get(offset..offset + 8)
            .ok_or_else(|| invalid("Truncated MP4"))?
            .try_into()
            .unwrap(),
    ))
}

#[derive(Clone)]
struct Atom {
    kind: [u8; 4],
    data: Vec<u8>,
}
impl Atom {
    fn children(&self) -> io::Result<Vec<Atom>> {
        atoms(&self.data)
    }
    fn encode(&self) -> io::Result<Vec<u8>> {
        let size =
            u32::try_from(self.data.len() + 8).map_err(|_| invalid("MP4 atom exceeds 4 GiB"))?;
        let mut out = size.to_be_bytes().to_vec();
        out.extend(self.kind);
        out.extend(&self.data);
        Ok(out)
    }
}
fn atoms(data: &[u8]) -> io::Result<Vec<Atom>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let size = u32_at(data, pos)?;
        let kind = data
            .get(pos + 4..pos + 8)
            .ok_or_else(|| invalid("Truncated atom"))?
            .try_into()
            .unwrap();
        let (size, header) = match size {
            0 => (data.len() - pos, 8),
            1 => (
                usize::try_from(u64_at(data, pos + 8)?).map_err(|_| invalid("Atom too large"))?,
                16,
            ),
            _ => (size as usize, 8),
        };
        if size < header || size > data.len() - pos {
            return Err(invalid("Invalid MP4 atom size"));
        }
        out.push(Atom {
            kind,
            data: data[pos + header..pos + size].to_vec(),
        });
        pos += size;
    }
    Ok(out)
}
fn pack(children: &[Atom]) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    for atom in children {
        data.extend(atom.encode()?);
    }
    Ok(data)
}
fn child(children: &[Atom], kind: &[u8; 4]) -> io::Result<Atom> {
    children
        .iter()
        .find(|x| &x.kind == kind)
        .cloned()
        .ok_or_else(|| invalid("Missing MP4 atom"))
}
fn replace(children: &mut Vec<Atom>, value: Atom) {
    // MP4 readers consume mvhd/tkhd before the tracks/media that depend on them.
    // Preserve the original atom order when updating an existing child.
    if let Some(index) = children.iter().position(|x| x.kind == value.kind) {
        children[index] = value;
    } else {
        children.push(value);
    }
}
fn movie(file: &mut (impl Read + Seek)) -> io::Result<(u64, u64, Atom)> {
    let len = file.seek(SeekFrom::End(0))?;
    let mut pos = 0;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos))?;
        let mut header = [0; 8];
        file.read_exact(&mut header)?;
        let mut size = u32_at(&header, 0)? as u64;
        let mut h = 8;
        if size == 1 {
            let mut ext = [0; 8];
            file.read_exact(&mut ext)?;
            size = u64::from_be_bytes(ext);
            h = 16;
        }
        if size == 0 {
            size = len - pos;
        }
        if size < h || size > len - pos {
            return Err(invalid("Invalid top-level atom"));
        }
        if &header[4..] == b"moof" {
            return Err(invalid("Fragmented DJI files are not supported"));
        }
        if &header[4..] == b"moov" {
            if size > 256 * 1024 * 1024 {
                return Err(invalid("Movie metadata exceeds 256 MiB"));
            }
            let mut data = vec![0; (size - h) as usize];
            file.read_exact(&mut data)?;
            return Ok((
                pos,
                size,
                Atom {
                    kind: *b"moov",
                    data,
                },
            ));
        }
        pos += size;
    }
    Err(invalid("No MP4 movie atom"))
}
fn sample_table(track: &Atom) -> io::Result<Vec<Atom>> {
    child(
        &child(&child(&track.children()?, b"mdia")?.children()?, b"minf")?.children()?,
        b"stbl",
    )?
    .children()
}
fn dji_kind(track: &Atom) -> Option<[u8; 4]> {
    let stsd = child(&sample_table(track).ok()?, b"stsd").ok()?;
    let kind: [u8; 4] = stsd.data.get(12..16)?.try_into().ok()?;
    if kind == *b"djmd" || kind == *b"dbgi" {
        Some(kind)
    } else {
        None
    }
}
pub fn has_dji_track(file: &mut (impl Read + Seek)) -> io::Result<bool> {
    let (_, _, moov) = movie(file)?;
    Ok(moov
        .children()?
        .iter()
        .any(|x| x.kind == *b"trak" && dji_kind(x) == Some(*b"djmd")))
}
fn samples(track: &Atom, file_len: u64) -> io::Result<Vec<(u64, u32)>> {
    let table = sample_table(track)?;
    let sizes = child(&table, b"stsz")?.data;
    let fixed = u32_at(&sizes, 4)?;
    let count = u32_at(&sizes, 8)? as usize;
    if count > 10_000_000 {
        return Err(invalid("Too many DJI samples"));
    }
    let sizes: Vec<u32> = (0..count)
        .map(|i| {
            if fixed != 0 {
                Ok(fixed)
            } else {
                u32_at(&sizes, 12 + i * 4)
            }
        })
        .collect::<io::Result<_>>()?;
    let chunks = table
        .iter()
        .find(|x| x.kind == *b"stco" || x.kind == *b"co64")
        .ok_or_else(|| invalid("No chunk offsets"))?;
    let chunk_count = u32_at(&chunks.data, 4)? as usize;
    let mapping = child(&table, b"stsc")?.data;
    let mapping_count = u32_at(&mapping, 4)? as usize;
    if mapping_count == 0 || u32_at(&mapping, 8)? != 1 {
        return Err(invalid("Invalid sample-to-chunk table"));
    }
    let mut entry = 0;
    let mut index = 0;
    let mut out = Vec::with_capacity(count);
    for c in 0..chunk_count {
        while entry + 1 < mapping_count && u32_at(&mapping, 8 + (entry + 1) * 12)? as usize <= c + 1
        {
            entry += 1;
        }
        let n = u32_at(&mapping, 12 + entry * 12)? as usize;
        if u32_at(&mapping, 16 + entry * 12)? != 1 {
            return Err(invalid("Multiple sample descriptions unsupported"));
        }
        let mut offset = if chunks.kind == *b"co64" {
            u64_at(&chunks.data, 8 + c * 8)?
        } else {
            u32_at(&chunks.data, 8 + c * 4)? as u64
        };
        for _ in 0..n {
            let size = *sizes
                .get(index)
                .ok_or_else(|| invalid("Inconsistent sample table"))?;
            if size > 16 * 1024 * 1024 || offset > file_len || size as u64 > file_len - offset {
                return Err(invalid("Invalid DJI sample range"));
            }
            out.push((offset, size));
            offset += size as u64;
            index += 1;
        }
    }
    if index != count {
        return Err(invalid("Incomplete sample table"));
    }
    Ok(out)
}

// Minimal protobuf wire reader/writer: retain every unmodified field's original bytes.
struct Field<'a> {
    number: u64,
    wire: u8,
    raw: &'a [u8],
    value: &'a [u8],
}
fn varint(data: &[u8], pos: &mut usize) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let b = *data
            .get(*pos)
            .ok_or_else(|| invalid("Truncated protobuf varint"))?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return Err(invalid("Overflowing protobuf varint"));
        }
        value |= ((b & 127) as u64) << shift;
        if b < 128 {
            return Ok(value);
        }
    }
    Err(invalid("Invalid protobuf varint"))
}
fn fields(data: &[u8]) -> io::Result<Vec<Field<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let start = pos;
        let key = varint(data, &mut pos)?;
        let wire = (key & 7) as u8;
        if key >> 3 == 0 {
            return Err(invalid("Invalid protobuf field number"));
        }
        let value_start;
        let len;
        match wire {
            0 => {
                value_start = pos;
                varint(data, &mut pos)?;
                len = pos - value_start;
            }
            1 => {
                value_start = pos;
                len = 8;
                pos += len;
            }
            2 => {
                len = usize::try_from(varint(data, &mut pos)?)
                    .map_err(|_| invalid("Oversized protobuf field"))?;
                value_start = pos;
                pos = pos
                    .checked_add(len)
                    .ok_or_else(|| invalid("Oversized protobuf field"))?;
            }
            5 => {
                value_start = pos;
                len = 4;
                pos += len;
            }
            _ => return Err(invalid("Unsupported protobuf wire type")),
        }
        if pos > data.len() {
            return Err(invalid("Truncated protobuf field"));
        }
        out.push(Field {
            number: key >> 3,
            wire,
            raw: &data[start..pos],
            value: &data[value_start..value_start + len],
        });
    }
    Ok(out)
}
fn put_varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 128 {
        out.push(v as u8 | 128);
        v >>= 7;
    }
    out.push(v as u8);
}
fn message(number: u64, data: &[u8], out: &mut Vec<u8>) {
    put_varint(number << 3 | 2, out);
    put_varint(data.len() as u64, out);
    out.extend(data);
}
fn float(number: u64, v: f32, out: &mut Vec<u8>) {
    put_varint(number << 3 | 5, out);
    out.extend(v.to_le_bytes());
}
fn scalar(number: u64, v: u64, out: &mut Vec<u8>) {
    put_varint(number << 3, out);
    put_varint(v, out);
}
fn float_value(field: &Field<'_>) -> io::Result<f64> {
    if field.wire != 5 {
        return Err(invalid("Expected protobuf float"));
    }
    Ok(f32::from_le_bytes(field.value.try_into().unwrap()) as f64)
}
fn nested<'a>(data: &'a [u8], number: u64) -> io::Result<Option<&'a [u8]>> {
    Ok(fields(data)?
        .into_iter()
        .find(|f| f.number == number && f.wire == 2)
        .map(|f| f.value))
}
fn uint(data: &[u8], number: u64) -> io::Result<u64> {
    match fields(data)?
        .into_iter()
        .find(|f| f.number == number && f.wire == 0)
    {
        Some(f) => varint(f.value, &mut 0),
        None => Ok(0),
    }
}
fn edit(
    data: &[u8],
    mut transform: impl FnMut(&Field<'_>) -> io::Result<Option<Vec<u8>>>,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    for f in fields(data)? {
        match transform(&f)? {
            Some(value) => out.extend(value),
            None => out.extend(f.raw),
        }
    }
    Ok(out)
}
#[derive(Default)]
struct Rewriter {
    readout_corrected: bool,
    first_ts: Option<u64>,
    sensor_fps: f64,
    fps: f64,
    corrected: usize,
}
impl Rewriter {
    fn rewrite(
        &mut self,
        data: &[u8],
        size: (u32, u32),
        camera: &impl Fn(f64) -> [f32; 4],
    ) -> io::Result<Vec<u8>> {
        if let Some(clip) = nested(data, 1)? {
            let header = nested(clip, 1)?.ok_or_else(|| invalid("No DJI clip header"))?;
            let proto = nested(header, 1)?.ok_or_else(|| invalid("No DJI schema name"))?;
            if !proto.starts_with(b"dvtm_ac103.") && !proto.starts_with(b"dvtm_wm169.") {
                return Err(invalid(
                    "DJI metadata export currently supports Action 2 / WM169 schemas",
                ));
            }
            if let Some(sensor) = nested(clip, 11)? {
                self.sensor_fps = fields(sensor)?
                    .iter()
                    .find(|f| f.number == 1)
                    .map(float_value)
                    .transpose()?
                    .unwrap_or(0.0);
            }
        }
        if let Some(stream) = nested(data, 2)? {
            if let Some(video) = nested(stream, 3)? {
                self.fps = fields(video)?
                    .iter()
                    .find(|f| f.number == 3)
                    .map(float_value)
                    .transpose()?
                    .unwrap_or(0.0);
            }
        }
        if !self.sensor_fps.is_finite()
            || !self.fps.is_finite()
            || self.sensor_fps <= 0.0
            || self.fps <= 0.0
        {
            return Err(invalid("Invalid DJI frame rates"));
        }
        edit(data, |f| {
            if f.wire != 2 {
                return Ok(None);
            }
            let value = match f.number {
                1 => edit(f.value, |c| {
                    // Original fisheye calibration does not describe the rendered pixels.
                    if c.number == 3 || c.number == 8 {
                        return Ok(Some(Vec::new()));
                    }
                    if c.number == 4 && self.readout_corrected {
                        let mut out = Vec::new();
                        message(4, &[], &mut out);
                        return Ok(Some(out));
                    }
                    Ok(None)
                })?,
                2 => edit(f.value, |s| {
                    if s.number != 3 || s.wire != 2 {
                        return Ok(None);
                    }
                    let mut value = Vec::new();
                    scalar(1, size.0 as u64, &mut value);
                    scalar(2, size.1 as u64, &mut value);
                    for v in fields(s.value)? {
                        if v.number != 1 && v.number != 2 {
                            value.extend(v.raw);
                        }
                    }
                    let mut out = Vec::new();
                    message(3, &value, &mut out);
                    Ok(Some(out))
                })?,
                3 => {
                    let header =
                        nested(f.value, 1)?.ok_or_else(|| invalid("No DJI frame header"))?;
                    if uint(header, 4)? != 0 {
                        return Err(invalid("Checksummed DJI frames unsupported"));
                    }
                    let ts = uint(header, 2)?;
                    let first = *self.first_ts.get_or_insert(ts);
                    let frame_ms = (ts as i128 - first as i128) as f64 / 1000.0;
                    edit(f.value, |frame| {
                        if frame.number == 2 && frame.wire == 2 {
                            // Orientation is baked into the exported image; remove original display orientation.
                            let camera = edit(frame.value, |c| {
                                Ok(if c.number == 7 {
                                    Some(Vec::new())
                                } else {
                                    None
                                })
                            })?;
                            let mut out = Vec::new();
                            message(2, &camera, &mut out);
                            return Ok(Some(out));
                        }
                        if frame.number != 3 || frame.wire != 2 {
                            return Ok(None);
                        }
                        let imu = edit(frame.value, |imu| {
                            if imu.number != 2 || imu.wire != 2 {
                                return Ok(None);
                            }
                            let fs = fields(imu.value)?;
                            let count = fs.iter().filter(|x| x.number == 3 && x.wire == 2).count();
                            let offset = fs
                                .iter()
                                .find(|x| x.number == 4)
                                .map(float_value)
                                .transpose()?
                                .unwrap_or(0.0);
                            if !offset.is_finite() {
                                return Err(invalid("Invalid DJI sample offset"));
                            }
                            let mut index = 0;
                            let attitude = edit(imu.value, |q| {
                                if q.number != 3 || q.wire != 2 {
                                    return Ok(None);
                                }
                                // Mirror telemetry-parser's DJI clock, including the sensor/video FPS ratio.
                                let ms = (frame_ms
                                    + (index as f64 - offset) / count as f64 * 1000.0
                                        / self.sensor_fps)
                                    / (self.fps / self.sensor_fps);
                                let quat = camera(ms);
                                index += 1;
                                self.corrected += 1;
                                let norm = quat
                                    .iter()
                                    .map(|x| *x as f64 * *x as f64)
                                    .sum::<f64>()
                                    .sqrt();
                                if !norm.is_finite() || norm < 1e-6 {
                                    return Err(invalid("Invalid stabilized quaternion"));
                                }
                                let mut value = Vec::new();
                                for (i, v) in quat.iter().enumerate() {
                                    float(i as u64 + 1, (*v as f64 / norm) as f32, &mut value);
                                }
                                for unknown in fields(q.value)? {
                                    if unknown.number > 4 {
                                        value.extend(unknown.raw);
                                    }
                                }
                                let mut out = Vec::new();
                                message(3, &value, &mut out);
                                Ok(Some(out))
                            })?;
                            let mut out = Vec::new();
                            message(2, &attitude, &mut out);
                            Ok(Some(out))
                        })?;
                        let mut out = Vec::new();
                        message(3, &imu, &mut out);
                        Ok(Some(out))
                    })?
                }
                _ => return Ok(None),
            };
            let mut out = Vec::new();
            message(f.number, &value, &mut out);
            Ok(Some(out))
        })
    }
}
fn track_id(track: &Atom) -> io::Result<u32> {
    let header = child(&track.children()?, b"tkhd")?;
    u32_at(
        &header.data,
        if header.data.first() == Some(&1) {
            20
        } else {
            12
        },
    )
}
fn movie_clock(moov: &Atom) -> io::Result<(u32, u64)> {
    let h = child(&moov.children()?, b"mvhd")?;
    if h.data.first() == Some(&1) {
        Ok((u32_at(&h.data, 20)?, u64_at(&h.data, 24)?))
    } else {
        Ok((u32_at(&h.data, 12)?, u32_at(&h.data, 16)? as u64))
    }
}
fn rebuild_track(
    track: &Atom,
    id: u32,
    duration: u64,
    offsets: &[u64],
    sizes: &[u32],
) -> io::Result<Atom> {
    let mut children = track.children()?;
    let mut header = child(&children, b"tkhd")?;
    let v1 = header.data.first() == Some(&1);
    if header.data.len() < if v1 { 36 } else { 24 } {
        return Err(invalid("Truncated DJI track header"));
    }
    let id_pos = if v1 { 20 } else { 12 };
    header.data[id_pos..id_pos + 4].copy_from_slice(&id.to_be_bytes());
    let duration_pos = if v1 { 28 } else { 20 };
    if v1 {
        header.data[duration_pos..duration_pos + 8].copy_from_slice(&duration.to_be_bytes());
    } else {
        header.data[duration_pos..duration_pos + 4].copy_from_slice(
            &u32::try_from(duration)
                .map_err(|_| invalid("DJI track duration overflow"))?
                .to_be_bytes(),
        );
    }
    replace(&mut children, header);
    let mut mdia = child(&children, b"mdia")?;
    let mut mdia_children = mdia.children()?;
    let mut minf = child(&mdia_children, b"minf")?;
    let mut minf_children = minf.children()?;
    let mut table = sample_table(track)?;
    table.retain(|x| {
        x.kind != *b"stco" && x.kind != *b"co64" && x.kind != *b"stsc" && x.kind != *b"stsz"
    });
    let mut chunks = vec![0; 4];
    chunks.extend((offsets.len() as u32).to_be_bytes());
    for v in offsets {
        chunks.extend(v.to_be_bytes());
    }
    table.push(Atom {
        kind: *b"co64",
        data: chunks,
    });
    let mut stsz = vec![0; 8];
    stsz.extend((sizes.len() as u32).to_be_bytes());
    for v in sizes {
        stsz.extend(v.to_be_bytes());
    }
    table.push(Atom {
        kind: *b"stsz",
        data: stsz,
    });
    let mut stsc = vec![0; 4];
    for _ in 0..4 {
        stsc.extend(1u32.to_be_bytes());
    }
    table.push(Atom {
        kind: *b"stsc",
        data: stsc,
    });
    replace(
        &mut minf_children,
        Atom {
            kind: *b"stbl",
            data: pack(&table)?,
        },
    );
    minf.data = pack(&minf_children)?;
    replace(&mut mdia_children, minf);
    mdia.data = pack(&mdia_children)?;
    replace(&mut children, mdia);
    // Track references and edit lists cannot refer to the source movie's track IDs/timeline.
    children.retain(|x| x.kind != *b"tref" && x.kind != *b"edts");
    Ok(Atom {
        kind: *b"trak",
        data: pack(&children)?,
    })
}

/// Append corrected DJI metadata without moving any video/audio bytes. Switch to
/// the new movie atom only after all data is written, leaving the old offsets valid.
pub fn preserve(
    input: &mut (impl Read + Seek),
    output: &mut (impl Read + Write + Seek),
    size: (u32, u32),
    readout_corrected: bool,
    camera: impl Fn(f64) -> [f32; 4],
) -> io::Result<usize> {
    let (_, _, source) = movie(input)?;
    let (old_pos, _, target) = movie(output)?;
    let (source_scale, source_duration) = movie_clock(&source)?;
    let (target_scale, target_duration) = movie_clock(&target)?;
    if source_scale == 0
        || target_scale == 0
        || (source_duration as f64 / source_scale as f64
            - target_duration as f64 / target_scale as f64)
            .abs()
            > 0.01
    {
        return Err(invalid(
            "DJI metadata requires a full-length export at the original speed",
        ));
    }
    let source_children = source.children()?;
    let mut target_children = target.children()?;
    target_children.sort_by_key(|x| if x.kind == *b"mvhd" { 0 } else { 1 });
    if source_children.iter().any(|x| x.kind == *b"mvex")
        || target_children.iter().any(|x| x.kind == *b"mvex")
    {
        return Err(invalid("Fragmented MP4 unsupported"));
    }
    target_children.retain(|x| x.kind != *b"trak" || dji_kind(x).is_none());
    let mut next_id = target_children
        .iter()
        .filter(|x| x.kind == *b"trak")
        .map(track_id)
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("Track ID overflow"))?;
    let file_len = input.seek(SeekFrom::End(0))?;
    let mut rewriter = Rewriter {
        readout_corrected,
        ..Rewriter::default()
    };
    let mut prepared = Vec::new();
    let mut data_size = 0u64;
    // Validate all payloads before touching the output. Retain only sample
    // ranges, keeping memory bounded for long clips with large dbgi tracks.
    for track in source_children.iter().filter(|x| x.kind == *b"trak") {
        let Some(kind) = dji_kind(track) else {
            continue;
        };
        let ranges = samples(track, file_len)?;
        for &(offset, len) in &ranges {
            if kind == *b"djmd" {
                input.seek(SeekFrom::Start(offset))?;
                let mut data = vec![0; len as usize];
                input.read_exact(&mut data)?;
                data_size += rewriter.rewrite(&data, size, &camera)?.len() as u64;
            } else {
                data_size += len as u64;
            }
        }
        prepared.push((track, kind, ranges));
    }
    if rewriter.corrected == 0 {
        return Err(invalid("No DJI attitude samples to correct"));
    }
    rewriter = Rewriter {
        readout_corrected,
        ..Rewriter::default()
    };
    output.seek(SeekFrom::End(0))?;
    output.write_all(&1u32.to_be_bytes())?;
    output.write_all(b"mdat")?;
    output.write_all(&(data_size + 16).to_be_bytes())?;
    for (track, kind, ranges) in prepared {
        let mut offsets = Vec::new();
        let mut sizes = Vec::new();
        for (offset, len) in ranges {
            input.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0; len as usize];
            input.read_exact(&mut data)?;
            if kind == *b"djmd" {
                data = rewriter.rewrite(&data, size, &camera)?;
            }
            offsets.push(output.stream_position()?);
            sizes.push(data.len() as u32);
            output.write_all(&data)?;
        }
        target_children.push(rebuild_track(
            track,
            next_id,
            target_duration,
            &offsets,
            &sizes,
        )?);
        next_id = next_id
            .checked_add(1)
            .ok_or_else(|| invalid("Track ID overflow"))?;
    }
    // DJI's maker atom and camera-authored metadata (including its thumbnails)
    // participate in camera-file identification. Keep source capture information;
    // merge ordinary output metadata such as comments into the native ilst.
    if let Some(source_udta) = source_children.iter().find(|x| x.kind == *b"udta") {
        let mut udta = target_children
            .iter()
            .find(|x| x.kind == *b"udta")
            .cloned()
            .unwrap_or(Atom {
                kind: *b"udta",
                data: Vec::new(),
            });
        let mut children = udta.children()?;
        for mut marker in source_udta.children()? {
            if marker.kind == *b"meta" {
                if marker.data.len() < 4 {
                    return Err(invalid("Truncated DJI metadata atom"));
                }
                let mut native = atoms(&marker.data[4..])?;
                if let Some(existing) = children.iter().find(|x| x.kind == *b"meta") {
                    if existing.data.len() >= 4 {
                        let existing_children = atoms(&existing.data[4..])?;
                        // Numeric-key mdta metadata has a different namespace.
                        if !existing_children.iter().any(|x| x.kind == *b"keys") {
                            if let (Ok(native_list), Ok(existing_list)) =
                                (child(&native, b"ilst"), child(&existing_children, b"ilst"))
                            {
                                let mut items = existing_list.children()?;
                                for item in native_list.children()? {
                                    replace(&mut items, item);
                                }
                                replace(
                                    &mut native,
                                    Atom {
                                        kind: *b"ilst",
                                        data: pack(&items)?,
                                    },
                                );
                            }
                        }
                    }
                }
                let mut data = marker.data[..4].to_vec();
                data.extend(pack(&native)?);
                marker.data = data;
            }
            // Do not copy unrelated user data or source timecode/movie atoms.
            if marker.kind == [0xa9, b'd', b'j', b'i'] || marker.kind == *b"meta" {
                replace(&mut children, marker);
            }
        }
        udta.data = pack(&children)?;
        replace(&mut target_children, udta);
    }
    let mut mvhd = child(&target_children, b"mvhd")?;
    let end = mvhd.data.len();
    if end < 4 {
        return Err(invalid("Truncated movie header"));
    }
    mvhd.data[end - 4..].copy_from_slice(&next_id.to_be_bytes());
    replace(&mut target_children, mvhd);
    let new_moov = Atom {
        kind: *b"moov",
        data: pack(&target_children)?,
    }
    .encode()?;
    output.write_all(&new_moov)?;
    output.flush()?;
    output.seek(SeekFrom::Start(old_pos + 4))?;
    output.write_all(b"free")?;
    output.flush()?;
    Ok(rewriter.corrected)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Vec<u8> {
        let mut header = Vec::new();
        message(1, b"dvtm_ac103.proto", &mut header);
        let mut clip = Vec::new();
        message(1, &header, &mut clip);
        let mut sensor = Vec::new();
        float(1, 50., &mut sensor);
        message(11, &sensor, &mut clip);
        message(3, b"obsolete-lens", &mut clip);
        message(8, b"obsolete-focal", &mut clip);
        let mut video = Vec::new();
        scalar(1, 4096, &mut video);
        scalar(2, 3072, &mut video);
        float(3, 50., &mut video);
        let mut stream = Vec::new();
        message(3, &video, &mut stream);
        let mut frame_header = Vec::new();
        scalar(2, 100_000, &mut frame_header);
        let mut attitude = Vec::new();
        float(4, 0.5, &mut attitude);
        for _ in 0..2 {
            let mut q = Vec::new();
            float(1, 0.5, &mut q);
            scalar(100, 77, &mut q);
            message(3, &q, &mut attitude);
        }
        let mut imu = Vec::new();
        message(2, &attitude, &mut imu);
        let mut frame = Vec::new();
        message(1, &frame_header, &mut frame);
        message(3, &imu, &mut frame);
        let mut result = Vec::new();
        message(1, &clip, &mut result);
        message(2, &stream, &mut result);
        message(3, &frame, &mut result);
        scalar(999, 123, &mut result);
        result
    }
    #[test]
    fn writes_sample_times_normalized_quaternions_and_output_geometry() {
        let times = std::cell::RefCell::new(Vec::new());
        let mut r = Rewriter::default();
        let out = r
            .rewrite(&fixture(), (1080, 1920), &|ms| {
                times.borrow_mut().push(ms);
                [2., 0., 0., 0.]
            })
            .unwrap();
        assert_eq!(*times.borrow(), vec![-5., 5.]);
        assert_eq!(r.corrected, 2);
        let clip = nested(&out, 1).unwrap().unwrap();
        assert!(nested(clip, 3).unwrap().is_none());
        assert!(nested(clip, 8).unwrap().is_none());
        let stream = nested(&out, 2).unwrap().unwrap();
        let video = nested(stream, 3).unwrap().unwrap();
        assert_eq!(uint(video, 1).unwrap(), 1080);
        assert_eq!(uint(video, 2).unwrap(), 1920);
        assert_eq!(uint(&out, 999).unwrap(), 123);
        let frame = nested(&out, 3).unwrap().unwrap();
        let imu = nested(frame, 3).unwrap().unwrap();
        let attitude = nested(imu, 2).unwrap().unwrap();
        for q in fields(attitude).unwrap().iter().filter(|x| x.number == 3) {
            assert_eq!(uint(q.value, 100).unwrap(), 77);
            assert_eq!(float_value(&fields(q.value).unwrap()[0]).unwrap(), 1.);
        }
    }
    #[test]
    fn rejects_invalid_pose_and_unsupported_schema() {
        assert!(
            Rewriter::default()
                .rewrite(&fixture(), (1, 1), &|_| [f32::NAN, 0., 0., 0.])
                .is_err()
        );
        let data = fixture();
        // Use byte replacement, preserving protobuf lengths and non-UTF8 fields.
        let mut bad_data = data.clone();
        let pos = data
            .windows(16)
            .position(|x| x == b"dvtm_ac103.proto")
            .unwrap();
        bad_data[pos..pos + 16].copy_from_slice(b"dvtm_oq101.proto");
        assert!(
            Rewriter::default()
                .rewrite(&bad_data, (1, 1), &|_| [1., 0., 0., 0.])
                .is_err()
        );
    }
    #[test]
    fn replacing_header_preserves_dependency_order() {
        let mut children = vec![
            Atom {
                kind: *b"mvhd",
                data: vec![1],
            },
            Atom {
                kind: *b"trak",
                data: vec![2],
            },
        ];
        replace(
            &mut children,
            Atom {
                kind: *b"mvhd",
                data: vec![3],
            },
        );
        assert_eq!(children[0].kind, *b"mvhd");
        assert_eq!(children[0].data, vec![3]);
        assert_eq!(children[1].kind, *b"trak");
    }
    #[test]
    fn rejects_truncated_and_overflowing_wire_data() {
        for bytes in [
            &[0x0a, 0xff][..],
            &[0x0d, 0][..],
            &[0][..],
            &[
                0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
            ][..],
        ] {
            assert!(fields(bytes).is_err());
        }
    }
    #[test]
    fn preserves_unknown_fields_byte_for_byte() {
        let mut data = Vec::new();
        scalar(123, 999, &mut data);
        message(456, b"opaque", &mut data);
        assert_eq!(edit(&data, |_| Ok(None)).unwrap(), data);
    }
    #[test]
    fn reads_constant_size_samples_across_multiple_chunks() {
        let a = |kind, values: &[u32]| Atom {
            kind,
            data: values.iter().flat_map(|v| v.to_be_bytes()).collect(),
        };
        let table = vec![
            a(*b"stsz", &[0, 10, 3]),
            a(*b"stco", &[0, 2, 100, 200]),
            a(*b"stsc", &[0, 2, 1, 2, 1, 2, 1, 1]),
        ];
        let stbl = Atom {
            kind: *b"stbl",
            data: pack(&table).unwrap(),
        };
        let minf = Atom {
            kind: *b"minf",
            data: stbl.encode().unwrap(),
        };
        let mdia = Atom {
            kind: *b"mdia",
            data: minf.encode().unwrap(),
        };
        let track = Atom {
            kind: *b"trak",
            data: mdia.encode().unwrap(),
        };
        assert_eq!(
            samples(&track, 300).unwrap(),
            vec![(100, 10), (110, 10), (200, 10)]
        );
        assert!(samples(&track, 205).is_err());
    }
}
