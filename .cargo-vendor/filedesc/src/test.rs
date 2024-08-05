use crate::FileDesc;
use assert2::{assert, let_assert};

#[test]
fn test_get_close_on_exec() {
	let fd = unsafe { FileDesc::duplicate_raw_fd(2i32).unwrap() };
	assert!(let Ok(true) = fd.get_close_on_exec());
	assert!(let Ok(()) = fd.set_close_on_exec(false));
	assert!(let Ok(false) = fd.get_close_on_exec());
	assert!(let Ok(_) = fd.duplicate());
}

#[test]
fn duplicate_convert_stdout() {
	let_assert!(Ok(fd) = FileDesc::duplicate_from(&std::io::stdout()));
	assert!(fd.as_raw_fd() != 0);
	assert!(fd.as_raw_fd() != 1);
	assert!(fd.as_raw_fd() != 2);

	let raw = fd.as_raw_fd();
	let fd = FileDesc::new(fd.into_fd());
	assert!(fd.as_raw_fd() == raw);
}
