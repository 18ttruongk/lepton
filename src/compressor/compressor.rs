use std::cmp::{max, min};

use super::brotli_encoder::BrotliEncoder;
use super::lepton_encoder::{LeptonData, LeptonEncoder};
use super::util::flush_lepton_data;
use interface::{
    Compressor, CumulativeOperationResult, ErrMsg, LeptonFlushResult, LeptonOperationResult,
    SimpleResult,
};
use primary_header::{serialize_header, HEADER_SIZE as PRIMARY_HEADER_SIZE};

pub struct LeptonCompressor {
    brotli_encoder: BrotliEncoder,
    lepton_encoder: Option<LeptonEncoder>,
    result: Option<Result<LeptonData, ErrMsg>>,
    start_byte: usize,
    embedding: usize,
    total_in: usize,
    pge: Vec<u8>,
    n_thread: u8,
    primary_header: Option<[u8; PRIMARY_HEADER_SIZE]>,
    primary_header_written: usize,
    brotli_done: bool,
    cmp_header_written: usize,
}

impl LeptonCompressor {
    pub fn new(start_byte: usize, embedding: usize) -> Self {
        LeptonCompressor {
            brotli_encoder: BrotliEncoder::new(),
            lepton_encoder: Some(LeptonEncoder::new(max(start_byte - embedding, 0))),
            result: None,
            start_byte,
            embedding,
            total_in: 0,
            pge: vec![],
            n_thread: 0,
            primary_header: None,
            primary_header_written: 0,
            brotli_done: false,
            cmp_header_written: 0,
        }
    }

    fn finish_lepton_encode(&mut self) -> SimpleResult<ErrMsg> {
        let result;
        self.result = Some(match self.lepton_encoder.take().unwrap().take_result() {
            Ok(data) => {
                self.n_thread = data.n_thread;
                let mut offset = 0;
                match self.brotli_encoder.encode_all(
                    &data.secondary_header,
                    &mut offset,
                    &mut [],
                    &mut 0,
                ) {
                    LeptonOperationResult::Failure(msg) => {
                        result = Err(msg.clone());
                        Err(msg)
                    }
                    _ => {
                        result = Ok(());
                        Ok(data)
                    }
                }
            }
            Err(e) => {
                result = Err(ErrMsg::JpegDecodeFail(e.clone()));
                Err(ErrMsg::JpegDecodeFail(e))
            }
        });
        result
    }
}

impl Compressor for LeptonCompressor {
    fn encode(
        &mut self,
        input: &[u8],
        input_offset: &mut usize,
        _output: &mut [u8],
        _output_offset: &mut usize,
    ) -> LeptonOperationResult {
        // Lepton encoder has finished. Compress GRB with Brotli.
        if let Some(ref mut result) = self.result {
            return match result {
                Ok(_) => {
                    while *input_offset < input.len() {
                        if let LeptonOperationResult::Failure(msg) =
                            self.brotli_encoder
                                .encode(input, input_offset, &mut [], &mut 0)
                        {
                            *result = Err(msg.clone());
                            return LeptonOperationResult::Failure(msg);
                        }
                    }
                    LeptonOperationResult::NeedsMoreInput
                }
                Err(msg) => LeptonOperationResult::Failure(msg.clone()),
            };
        }

        // Run Lepton encoder
        let mut result = LeptonOperationResult::NeedsMoreInput;
        while *input_offset < input.len() {
            let mut lepton_finish = false;
            let old_input_offset = *input_offset;
            if self.total_in < self.embedding {
                let dist_to_embedding = self.embedding - self.total_in;
                let consume_end = min(old_input_offset + dist_to_embedding, input.len());
                if self.start_byte < self.embedding
                    && self.start_byte < self.total_in + input.len() - old_input_offset
                {
                    let dist_to_start = self.start_byte - self.total_in;
                    let pge_start = old_input_offset + max(dist_to_start, 0);
                    self.pge.extend(&input[pge_start..consume_end]);
                }
                *input_offset = consume_end;
            } else {
                if !self.pge.is_empty() {
                    self.lepton_encoder.as_mut().unwrap().add_pge(&self.pge);
                    self.pge.clear();
                }
                if self.lepton_encoder
                    .as_mut()
                    .unwrap()
                    .encode(input, input_offset)
                    == CumulativeOperationResult::Finish
                {
                    if let Err(msg) = self.finish_lepton_encode() {
                        result = LeptonOperationResult::Failure(msg);
                    }
                    lepton_finish = true;
                }
            };
            self.total_in += *input_offset - old_input_offset;
            if lepton_finish {
                break;
            }
        }
        result
    }

    fn flush(&mut self, output: &mut [u8], output_offset: &mut usize) -> LeptonFlushResult {
        match self.result {
            Some(Err(ref msg)) => return LeptonFlushResult::Failure(msg.clone()),
            None => {
                self.lepton_encoder.as_mut().unwrap().flush();
                if let Err(msg) = self.finish_lepton_encode() {
                    return LeptonFlushResult::Failure(msg);
                }
            }
            _ => (),
        }
        while *output_offset < output.len() {
            match self.primary_header {
                None => match self.brotli_encoder.finish_encode() {
                    Ok(size) => {
                        self.primary_header = Some(serialize_header(
                            if self.start_byte <= self.embedding {
                                b'N'
                            } else {
                                b'Y'
                            },
                            self.n_thread,
                            &[0; 12],
                            self.total_in,
                            size,
                        ));
                    }
                    Err(msg) => {
                        self.result = Some(Err(msg.clone()));
                        return LeptonFlushResult::Failure(msg);
                    }
                },
                Some(ref header) => {
                    if let Some(result) = flush_lepton_data(
                        output,
                        output_offset,
                        header,
                        &mut self.brotli_encoder,
                        &mut self.result.as_mut().unwrap().as_mut().unwrap().cmp,
                        &mut self.primary_header_written,
                        &mut self.brotli_done,
                        &mut self.cmp_header_written,
                    ) {
                        if let LeptonFlushResult::Failure(ref msg) = result {
                            self.result = Some(Err(msg.clone()));
                        }
                        return result;
                    }
                }
            }
        }
        LeptonFlushResult::NeedsMoreOutput
    }
}
