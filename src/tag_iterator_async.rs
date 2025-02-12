use std::io::ErrorKind;
use std::iter::repeat;
use std::mem;
use ebml_iterable_specification::{EbmlSpecification, EbmlTag, Master, TagDataType};
use futures::{AsyncRead, AsyncReadExt, Stream};
use crate::error::{TagIteratorError, ToolError};
use crate::tag_iterator_util::{EBMLSize, ProcessingTag};
use crate::tag_iterator_util::EBMLSize::{Known, Unknown};
use crate::tag_iterator_util::ProcessingTag::{EndTag, NextTag};
use crate::tools;

///
/// This Can be transformed into a [`Stream`] using [`into_stream`], or consumed directly by calling [`.next().await`] in a loop.
///
/// The struct can be created with the [`new()`] function on any source that implements the [`futures::AsyncRead`] trait.
///
pub struct TagIteratorAsync<R: AsyncRead + Unpin, TSpec>
    where
        TSpec: EbmlSpecification<TSpec> + EbmlTag<TSpec> + Clone
{
    read: R,
    buf: Vec<u8>,
    offset: usize,
    tag_stack: Vec<ProcessingTag<TSpec>>
}

impl<R: AsyncRead + Unpin, TSpec> TagIteratorAsync<R, TSpec>
    where
        TSpec: EbmlSpecification<TSpec> + EbmlTag<TSpec> + Clone
{

    pub fn new(read: R) -> Self {
        Self {
            read,
            buf: Default::default(),
            offset: 0,
            tag_stack: Default::default()
        }
    }

    fn current_offset(&self) -> usize {
        self.offset
    }

    fn advance(&mut self, length: usize) {
        self.offset += length;
        self.buf.drain(0..length);
    }

    fn advance_get(&mut self, length: usize) -> Vec<u8> {
        self.offset += length;
        let upper = self.buf.split_off(length);
        mem::replace(&mut self.buf, upper)
    }

    async fn ensure_data_read(&mut self, len: usize) -> Result<bool, TagIteratorError> {
        let size = self.buf.len();
        if size < len {
            let remaining = len - size;
            self.buf.extend(repeat(0).take(remaining));
            match self.read.read_exact(&mut self.buf[size..]).await {
                Err(source) => {
                    return match source.kind() {
                        ErrorKind::UnexpectedEof => {
                            Ok(false)
                        }
                        _ => Err(TagIteratorError::ReadError { source })?
                    }

                }
                _ => {}
            }
        }
        Ok(true)
    }

    async fn read_tag_id(&mut self) -> Result<u64, TagIteratorError> {
        self.ensure_data_read(8).await?;
        match tools::read_vint(&self.buf).map_err(|e| TagIteratorError::CorruptedFileData(e.to_string()))? {
            Some((value, length)) => {
                self.advance(length);
                Ok(value + (1 << (7 * length)))
            },
            None => Err(TagIteratorError::CorruptedFileData(String::from("Expected tag id, but reached end of source."))),
        }
    }

    async fn read_tag_size(&mut self) -> Result<EBMLSize, TagIteratorError> {
        self.ensure_data_read(8).await?;
        match tools::read_vint(&self.buf).map_err(|e| TagIteratorError::CorruptedFileData(e.to_string()))? {
            Some((value, length)) => {
                self.advance(length);
                Ok(value.into())
            },
            None => Err(TagIteratorError::CorruptedFileData(String::from("Expected tag size, but reached end of source."))),
        }
    }

    async fn read_tag_data(&mut self, size: usize) -> Result<Vec<u8>, TagIteratorError> {
        if !self.ensure_data_read(size).await? {
            return Err(TagIteratorError::CorruptedFileData(String::from("reached end of file but expecting more data")));
        }
        Ok(self.advance_get(size))
    }

    async fn read_tag(&mut self) -> Result<TSpec, TagIteratorError> {
        let tag_id = self.read_tag_id().await?;
        let spec_tag_type = TSpec::get_tag_data_type(tag_id);
        let size = self.read_tag_size().await?;

        let is_master = matches!(spec_tag_type, TagDataType::Master);
        let is_child = self.tag_stack.last().map(|it| {
            match it {
                NextTag {..} => true,
                EndTag { size, tag: parent, .. } => {
                    // The unknown check is there to still support proper parsing of badly formatted files.
                    *size != Unknown || parent.is_child(tag_id)
                }
            }
        }).unwrap_or(true);
        if is_master {
            let end_tag = EndTag {
                tag: TSpec::get_master_tag(tag_id, Master::End).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was master, but could not get tag!", tag_id)),
                size,
                start: self.current_offset(),
            };
            let start_tag = TSpec::get_master_tag(tag_id, Master::Start).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was master, but could not get tag!", tag_id));
            if is_child {
                self.tag_stack.push(end_tag);
                Ok(start_tag)
            } else {
                let tag = mem::replace(self.tag_stack.last_mut().unwrap(), end_tag).into_inner();
                self.tag_stack.push(NextTag { tag: start_tag });
                Ok(tag)
            }
        } else {
            let size = if let Known(size) = size {
                size
            } else {
                return Err(TagIteratorError::CorruptedFileData("Unknown size for primitive not allowed".into()));
            };
            let raw_data = self.read_tag_data(size).await?;
            let tag = match spec_tag_type {
                TagDataType::Master => { unreachable!("Master should have been handled before querying data") },
                TagDataType::UnsignedInt => {
                    let val = tools::arr_to_u64(&raw_data).map_err(|e| TagIteratorError::CorruptedTagData{ tag_id, problem: e })?;
                    TSpec::get_unsigned_int_tag(tag_id, val).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was unsigned int, but could not get tag!", tag_id))
                },
                TagDataType::Integer => {
                    let val = tools::arr_to_i64(&raw_data).map_err(|e| TagIteratorError::CorruptedTagData{ tag_id, problem: e })?;
                    TSpec::get_signed_int_tag(tag_id, val).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was integer, but could not get tag!", tag_id))
                },
                TagDataType::Utf8 => {
                    let val = String::from_utf8(raw_data.to_vec()).map_err(|e| TagIteratorError::CorruptedTagData{ tag_id, problem: ToolError::FromUtf8Error(raw_data, e) })?;
                    TSpec::get_utf8_tag(tag_id, val).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was utf8, but could not get tag!", tag_id))
                },
                TagDataType::Binary => {
                    TSpec::get_binary_tag(tag_id, &raw_data).unwrap_or_else(|| TSpec::get_raw_tag(tag_id, &raw_data))
                },
                TagDataType::Float => {
                    let val = tools::arr_to_f64(&raw_data).map_err(|e| TagIteratorError::CorruptedTagData{ tag_id, problem: e })?;
                    TSpec::get_float_tag(tag_id, val).unwrap_or_else(|| panic!("Bad specification implementation: Tag id {} type was float, but could not get tag!", tag_id))
                },
            };
            if is_child {
                Ok(tag)
            } else {
                Ok(mem::replace(self.tag_stack.last_mut().unwrap(), NextTag { tag }).into_inner())
            }
        }
    }

    /// can be consumed
    pub async fn next(&mut self) -> Option<Result<TSpec, TagIteratorError>> {
        if let Some(tag) = self.tag_stack.pop() {
            match tag {
                EndTag { size, start, tag } => {
                    if let Known(size) = size {
                        if self.current_offset() >= start + size {
                            return Some(Ok(tag));
                        }
                    }
                    self.tag_stack.push(EndTag { size, start, tag });
                },
                NextTag { tag } => return Some(Ok(tag))
            }
        }
        match self.ensure_data_read(1).await {
            Err(err) => return Some(Err(err)),
            Ok(data_remaining) => {
                if !data_remaining {
                    return if let Some(tag) = self.tag_stack.pop() {
                        Some(Ok(tag.into_inner()))
                    } else {
                        None
                    }
                }
            }
        }
        Some(self.read_tag().await)
    }

    pub fn into_stream(self) -> impl Stream<Item=Result<TSpec, TagIteratorError>> {
        futures::stream::unfold(self, |mut read| async {
            let next = read.next().await;
            next.map(move |it| (it, read))
        })
    }
}
