use std::{
	fs::File,
	io::{Read, Seek, SeekFrom, Write},
	path::Path
};

use kdam::BarExt;
use ndarray::{Array1, Array2, ArrayViewD, Axis, concatenate, s};
use ort::{
	execution_providers::CUDAExecutionProvider,
	memory::Allocator,
	session::{Session, builder::SessionBuilder},
	training::{Checkpoint, Trainer}
};
use rand::RngCore;
use tokenizers::Tokenizer;

const BATCH_SIZE: usize = 16;
const SEQUENCE_LENGTH: usize = 256;

fn main() -> ort::Result<()> {
	tracing_subscriber::fmt::init();

	ort::init().commit()?;

	kdam::term::init(true);
	let _ = kdam::term::hide_cursor();

	let trainer = Trainer::new(
		SessionBuilder::new()?.with_execution_providers([CUDAExecutionProvider::default().build()])?,
		Allocator::default(),
		Checkpoint::load("tools/train-data/mini-clm/checkpoint")?,
		"tools/train-data/mini-clm/training_model.onnx",
		"tools/train-data/mini-clm/eval_model.onnx",
		"tools/train-data/mini-clm/optimizer_model.onnx"
	)?;

	let tokenizer = Tokenizer::from_file(
		Path::new(env!("CARGO_MANIFEST_DIR"))
			.parent()
			.unwrap()
			.join("gpt2")
			.join("data")
			.join("tokenizer.json")
	)
	.unwrap();

	let optimizer = trainer.optimizer();
	optimizer.set_lr(7e-5)?;

	let mut dataset = File::open("dataset.bin").unwrap();
	let file_size = dataset.metadata().unwrap().len();
	let num_tokens = (file_size / 2) as usize; // 16-bit tokens
	let mut rng = rand::thread_rng();

	let mut input_buffer = vec![0u16; SEQUENCE_LENGTH * BATCH_SIZE];
	let mut label_buffer = vec![0u16; SEQUENCE_LENGTH * BATCH_SIZE];
	let mut pb = kdam::tqdm!(total = 5000);
	for _ in 0..5000 {
		for batch in 0..BATCH_SIZE {
			let start_idx = rng.next_u64() % (num_tokens - SEQUENCE_LENGTH - 1) as u64;
			dataset.seek(SeekFrom::Start(start_idx * 2)).unwrap();
			dataset
				.read_exact(unsafe {
					std::slice::from_raw_parts_mut(
						input_buffer[batch * SEQUENCE_LENGTH..(batch + 1) * SEQUENCE_LENGTH]
							.as_mut_ptr()
							.cast::<u8>(),
						SEQUENCE_LENGTH * 2
					)
				})
				.unwrap();
			dataset.seek(SeekFrom::Start((start_idx + 1) * 2)).unwrap();
			dataset
				.read_exact(unsafe {
					std::slice::from_raw_parts_mut(
						label_buffer[batch * SEQUENCE_LENGTH..(batch + 1) * SEQUENCE_LENGTH]
							.as_mut_ptr()
							.cast::<u8>(),
						SEQUENCE_LENGTH * 2
					)
				})
				.unwrap();
		}

		let inputs = Array2::<i64>::from_shape_vec([BATCH_SIZE, SEQUENCE_LENGTH], input_buffer.iter().map(|c| *c as i64).collect()).unwrap();
		let labels = Array1::<i64>::from_shape_vec([BATCH_SIZE * SEQUENCE_LENGTH], label_buffer.iter().map(|c| *c as i64).collect()).unwrap();

		let outputs = trainer.step(ort::inputs![inputs.view()]?, ort::inputs![labels.view()]?)?;
		let loss = outputs[0].try_extract_scalar::<f32>()?;
		pb.set_postfix(format!("loss={loss:.3}"));
		pb.update(1).unwrap();
		if loss.is_nan() {
			return Ok(());
		}
		optimizer.step()?;
		optimizer.reset_grad()?;
	}

	eprintln!();
	let _ = kdam::term::show_cursor();

	trainer.export("trained-clm.onnx", ["probs"])?;

	let session = Session::builder()?.commit_from_file("trained-clm.onnx")?;

	let mut stdout = std::io::stdout();

	let tokens = tokenizer.encode("<|endoftext|>", false).unwrap();
	let tokens = tokens.get_ids().iter().map(|i| *i as i64).collect::<Vec<_>>();

	let mut tokens = Array1::from_iter(tokens.iter().cloned());

	for _ in 0..50 {
		let array = tokens.view().insert_axis(Axis(0));
		let outputs = session.run(ort::inputs![array]?)?;
		let generated_tokens: ArrayViewD<f32> = outputs["probs"].try_extract_tensor()?;

		let probabilities = &mut generated_tokens
			.slice(s![-1, ..])
			.to_owned()
			.iter()
			.cloned()
			.enumerate()
			.collect::<Vec<_>>();
		probabilities.sort_unstable_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Less));

		let token = probabilities[0].0;
		tokens = concatenate![Axis(0), tokens, ndarray::array![token.try_into().unwrap()]];

		let token_str = tokenizer.decode(&[token as _], false).unwrap();
		print!("{}", token_str);
		stdout.flush().unwrap();
	}

	println!();
	Ok(())
}
