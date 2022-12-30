use crate::{format_clue, scanner::CharExt, check, CodeChar, Code, CodeExt};
use ahash::AHashMap;
use utf8_decode::{Decoder, decode};
use std::{
	collections::linked_list::Iter,
	env,
	iter::{Peekable, Rev},
	str,
	path::Path,
	ffi::OsStr,
	fmt::Display,
	fs::File,
	io::{self, BufRead, BufReader, Read, ErrorKind},
};

pub type LinkedString = std::collections::LinkedList<CodeChar>;
pub type PPVars = AHashMap<Vec<u8>, PPVar>;
type CodeChars<'a, 'b> = &'a mut Peekable<std::slice::Iter<'b, CodeChar>>;

impl CodeExt for LinkedString {
	fn to_string(&self) -> String {
		let mut result = String::new();
        for (c, _) in self {
            result.push(*c)
        }
        result
	}

	fn from_str(chars: &str, line: usize) -> Self {
        let mut result = Self::new();
        for c in chars.chars() {
            result.push_back((c, line));
        }
        result
    }
}

pub enum PPVar {
	Simple(LinkedString)
}

fn error(msg: impl Into<String>, line: usize, filename: &String) -> String {
	println!("Error in file \"{filename}\" at line {line}!");
	msg.into()
}

fn skip_whitespace_backwards(code: &mut Peekable<Rev<Iter<CodeChar>>>) {
	while let Some((c, _)) = code.peek() {
		if c.is_whitespace() {
			code.next();
		} else {
			break;
		}
	}
}

fn read_pseudos(mut code: Peekable<Rev<Iter<CodeChar>>>) -> Vec<LinkedString> {
	let mut newpseudos: Vec<LinkedString> = Vec::new();
	while {
		if let Some((c, _)) = code.next() {
			if *c == '=' {
				if let Some((c, _)) = code.next() {
					matches!(c, '!' | '=')
				} else {
					return newpseudos;
				}
			} else {
				true
			}
		} else {
			return newpseudos;
		}
	} {}
	skip_whitespace_backwards(&mut code);
	while {
		let mut name = LinkedString::new();
		while {
			if let Some((c, _)) = code.peek() {
				c.is_identifier()
			} else {
				false
			}
		} {
			name.push_front(*code.next().unwrap())
		}
		newpseudos.push(name);
		skip_whitespace_backwards(&mut code);
		if let Some((c, _)) = code.next() {
			*c == ','
		} else {
			false
		}
	} {}
	newpseudos
}

pub struct PreProcessor<'a, 'b> {
	values: &'a PPVars,
	filename: &'b String
}

impl<'a, 'b> PreProcessor<'a, 'b> {
	pub fn start(
		rawcode: Code,
		values: &'a PPVars,
		filename: &'b String
	) -> Result<(LinkedString, bool), String> {
		(Self { values, filename }).preprocess_code(rawcode, &mut 1, None)
	}

	fn expected(&self, expected: &str, got: &str, line: usize) -> String {
		error(
			format_clue!("Expected '", expected, "', got '", got, "'"),
			line,
			self.filename,
		)
	}

	fn expected_before(&self, expected: &str, before: &str, line: usize) -> String {
		error(
			format_clue!("Expected '", expected, "' before '", before, "'"),
			line,
			self.filename,
		)
	}

	fn skip_whitespace(&mut self, chars: CodeChars, line: &mut usize) {
		while let Some((c, _)) = chars.peek() {
			if c.is_whitespace() {
				if *c == '\n' {
					*line += 1;
				}
				chars.next();
			} else {
				break;
			}
		}
	}

	fn reach(&mut self, chars: CodeChars, end: char, line: &mut usize) -> Result<(), String> {
		self.skip_whitespace(chars, line);
		if let Some((c, _)) = chars.next() {
			if end != *c {
				Err(self.expected(&end.to_string(), &c.to_string(), *line))
			} else {
				Ok(())
			}
		} else {
			Err(self.expected_before(&end.to_string(), "<end>", *line))
		}
	}

	fn read_with(&mut self, chars: CodeChars, mut f: impl FnMut(&(char, usize)) -> bool) -> Code {
		let mut result = Code::new();
		while {
			if let Some(c) = chars.peek() {
				f(c)
			} else {
				false
			}
		} {
			result.push(*chars.next().unwrap())
		}
		result
	}

	fn read_word(&mut self, chars: CodeChars) -> Code {
		self.read_with(chars, |(c, _)| !c.is_whitespace())
	}

	fn assert_word(&mut self, chars: CodeChars, line: &mut usize) -> Result<Code, String> {
		self.skip_whitespace(chars, line);
		let word = self.read_word(chars);
		if word.is_empty() {
			Err(error("Word expected", *line, self.filename))
		} else {
			Ok(word)
		}
	}

	fn read_until(&mut self, chars: CodeChars, end: char, line: &mut usize) -> Result<Code, String> {
		let arg = self.read_with(chars, |(c, _)| *c != end);
		if chars.next().is_none() {
			return Err(self.expected_before(&end.to_string(), "<end>", *line));
		}
		Ok(arg)
	}

	fn read_arg(&mut self, chars: CodeChars, line: &mut usize) -> Result<(LinkedString, bool), String> {
		self.reach(chars, '"', line)?;
		let rawarg = self.read_until(chars, '"', line)?;
		let (arg, result) = self.preprocess_code(rawarg, line, None)?;
		Ok((arg, result))
	}

	fn read_block(&mut self, chars: CodeChars, line: &mut usize) -> Result<(usize, Code), String> {
		self.reach(chars, '{', line)?;
		let mut block = Code::new();
		let mut cscope = 1u8;
		for c in chars.by_ref() {
			block.push(*c);
			match c.0 {
				'{' => cscope += 1,
				'}' => {
					cscope -= 1;
					if cscope == 0 {
						block.pop();
						return Ok((*line, block));
					}
				}
				_ => {}
			}
		}
		Err(self.expected_before("}", "<end>", *line))
	}

	fn keep_block(
		&mut self,
		chars: CodeChars,
		line: &mut usize,
		code: &mut LinkedString,
		cond: bool
	) -> Result<bool, String> {
		let (mut line, block) = self.read_block(chars, line)?;
		code.append(&mut if cond {
			self.preprocess_code(block, &mut line, None)?.0
		} else {
			let mut lines = LinkedString::new();
			for c in block {
				if c.0 == '\n' {
					lines.push_back(c);
				}
			}
			lines
		});
		Ok(cond)
	}

	pub fn preprocess_code(
		&mut self,
		rawcode: Code,
		line: &mut usize,
		mut pseudos: Option<Vec<LinkedString>>,
	) -> Result<(LinkedString, bool), String> {
		let mut code = LinkedString::new();
		let mut prev = true;
		let mut prevline = *line;
		let mut chars = rawcode.iter().peekable();
		while let Some((c, _)) = chars.next() {
			match c {
				'\n' => {
					for _ in 0..=*line - prevline {
						code.push_back(('\n', *line));
					}
					*line += 1;
					prevline = *line;
				}
				'@' => {
					let directive = self.read_word(&mut chars);
					prev = match directive.to_string().as_str() {
						"ifos" => {
							let target_os = self.assert_word(&mut chars,line)?
								.to_string()
								.to_ascii_lowercase();
							self.keep_block(&mut chars, line, &mut code, env::consts::OS == target_os)?
						}
						"ifdef" => {
							let var = self.assert_word(&mut chars, line)?.to_string();
							let conditon =
								env::var(&var).is_ok() || self.values.contains_key(&var.into_bytes());
							self.keep_block(&mut chars, line, &mut code, conditon)?
						}
						"ifcmp" => {
							let arg1 = self.read_arg(&mut chars, line)?.0;
							let condition = self.assert_word(&mut chars, line)?;
							let arg2 = self.read_arg(&mut chars, line)?.0;
							let result = match condition.to_string().as_str() {
								"==" => arg1 == arg2,
								"!=" => arg1 != arg2,
								_ => return Err(self.expected("==", &condition.to_string(), *line)),
							};
							self.keep_block(&mut chars, line, &mut code, result)?
						}
						"if" => todo!(),
						"else" => self.keep_block(&mut chars, line, &mut code, !prev)?,
						"error" => {
							let msg = self.read_arg(&mut chars, line)?.0;
							return Err(error(msg.to_string(), *line, self.filename));
						}
						"warning" => {
							let (msg, result) = self.read_arg(&mut chars, line)?;
							println!("Warning: \"{}\"", msg.to_string());
							result
						}
						"print" => {
							let (msg, result) = self.read_arg(&mut chars, line)?;
							println!("{}", msg.to_string());
							result
						}
						"execute" => todo!(),
						"eval" => todo!(),
						"include" => todo!(),
						"macro" => todo!(),
						"" => return Err(error("Expected directive name", *line, self.filename)),
						_ => {
							return Err(error(
								format_clue!("Unknown directive '", directive.to_string(), "'"),
								*line,
								self.filename,
							))
						}
					};
				}
				'$' => {
					let name = {
						let name = self.read_with(&mut chars, |(c, _)| c.is_identifier());
						if name.is_empty() {
							String::from("1")
						} else {
							name.to_string()
						}
					};
					if let Ok(index) = name.parse::<usize>() {
						if pseudos.is_none() {
							pseudos = Some(read_pseudos(code.iter().rev().peekable()));
						}
						let pseudos = pseudos.as_ref().unwrap();
						let mut var = pseudos
							.get(pseudos.len() - index)
							.cloned()
							.unwrap_or_else(|| LinkedString::from_str("nil", *line));
						code.append(&mut var);
					} else {
						let name_bytes;
						let mut value = if let Ok(value) = env::var(&name) {
							LinkedString::from_str(&value, *line)
						} else if let Some(PPVar::Simple(value)) = self.values.get({
							name_bytes = name.into_bytes();
							&name_bytes
						}) {
							value.clone()
						} else {
							let name = check!(String::from_utf8(name_bytes));
							return Err(error(
								format_clue!("Value '", name, "' not found"),
								*line,
								self.filename,
							));
						};
						code.append(&mut value);
					}
				}
				'\'' | '"' | '`' => {
					code.push_back((*c, *line));
					while let Some((stringc, _)) = chars.next() {
						if *stringc == '\n' {
							*line += 1;
							prevline += 1;
						} else if *stringc == '\\' {
							if let Some((nextc, _)) = chars.peek() {
								if nextc == c {
									code.push_back((*stringc, *line));
									code.push_back((*nextc, *line));
									chars.next();
									continue;
								}
							}
						}
						code.push_back((*stringc, *line));
						if stringc == c {
							break
						}
					}
				}
				'=' => {
					code.push_back(('=', *line));
					if let Some((nc, _)) = chars.peek() {
						if matches!(nc, '=' | '>') {
							code.push_back(*chars.next().unwrap());
						} else {
							pseudos = None;
						}
					}
				}
				'!' | '>' | '<' => {
					code.push_back((*c, *line));
					if let Some((nc, _)) = chars.peek() {
						if *nc == '=' {
							code.push_back(*chars.next().unwrap());
						}
					}
				}
				_ => code.push_back((*c, *line)),
			}
		}
		Ok((code, prev))
	}
}

struct PeekableBufReader<R> {
	buffer: BufReader<R>,
	peeked: Option<u8>,
}

impl<R: Read> PeekableBufReader<R> {
	fn new(inner: R) -> Self {
		Self {
			buffer: BufReader::new(inner),
			peeked: None
		}
	}

	fn skip_whitespace(&mut self, line: &mut usize, finalcode: &mut Code) -> io::Result<()> {
		while let Some(c) = self.peek_char()? {
			if c.is_ascii_whitespace() {
				if c == '\n' {
					finalcode.push(('\n', *line));
					*line += 1;
				}
				self.read_char()?;
			} else {
				break;
			}
		}
		Ok(())
	}

	fn read_byte(&mut self) -> io::Result<Option<u8>> {
		if self.peeked.is_some() {
			let peeked = self.peeked;
			self.peeked = None;
			Ok(peeked)
		} else {
			let mut buf = [0];
			match self.buffer.read_exact(&mut buf) {
				Ok(_) => {
					Ok(Some(buf[0]))
				}
				Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
				Err(e) => return Err(e)
			}
		}
	}

	fn peek_byte(&mut self) -> io::Result<Option<u8>> {
		if self.peeked.is_none() {
			self.peeked = self.read_byte()?;
		}
		Ok(self.peeked)
	}

	fn read_char(&mut self) -> io::Result<Option<char>> {
		Ok(self.read_byte()?.map(|byte| byte as char))
	}

	fn peek_char(&mut self) -> io::Result<Option<char>> {
		Ok(self.peek_byte()?.map(|byte| byte as char))
	}

	fn read_identifier(&mut self, line: usize, filename: &String) -> io::Result<Vec<u8>> {
		let mut ident = Vec::new();
		while let Some(c) = self.peek_char()? {
			match c {
				_ if c.is_ascii_whitespace() => break,
				_ if c.is_identifier() => ident.push(self.read_byte()?.unwrap()),
				_ => return Err(analyze_error(format!("Invalid name character '{c}'"), line, filename))
			}
		}
		Ok(ident)
	}
}

impl<R: Read> Read for PeekableBufReader<R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		self.buffer.read(buf)
	}
}

impl<R: Read> BufRead for PeekableBufReader<R> {
	fn fill_buf(&mut self) -> io::Result<&[u8]> {
		self.buffer.fill_buf()
	}

	fn consume(&mut self, amt: usize) {
		self.buffer.consume(amt)
	}
}

fn analyze_error(msg: impl Into<String>, line: usize, filename: &String) -> io::Error {
	io::Error::new(io::ErrorKind::Other, error(msg.into(), line, filename))
}

fn add_newlines(code: &mut Code, newlines: Vec<u8>, line: &mut usize) {
	for c in newlines {
		if c == b'\n' {
			code.push(('\n', *line));
			*line += 1;
		}
	}
}

pub fn analyze_file<P: AsRef<Path>>(
	path: P,
	filename: &String,
) -> Result<(Code, Option<PPVars>), io::Error>
where
	P: AsRef<OsStr> + Display,
{
	let file = File::open(path)?;
	let len = file.metadata()?.len() as usize;
	analyze_code(file, len, filename)
}

pub fn analyze_code<R: Read>(
	code: R,
	len: usize,
	filename: &String,
) -> Result<(Code, Option<PPVars>), io::Error> {
	let mut finalcode = Code::with_capacity(len);
	let mut code = PeekableBufReader::new(code);
	let mut line = 1usize;
	let mut variables = None;
	while let Some(c) = code.read_char()? {
		if match c {
			'\n' => {line += 1; true}
			'@' => {
				if variables.is_none() {
					variables = Some(AHashMap::new());
				}
				let variables = variables.as_mut().unwrap();
				let mut directive = Vec::new();
				code.read_until(b' ', &mut directive)?;
				match directive[..] {
					[b'd', b'e', b'f', b'i', b'n', b'e'] => {
						code.skip_whitespace(&mut line, &mut finalcode)?;
						let name = code.read_identifier(line, filename)?;
						let mut value = String::new();
						code.read_line(&mut value)?;
						let value = LinkedString::from_str(value.trim(), line);
						variables.insert(name, PPVar::Simple(value));
					}
					_ => {
						finalcode.push(('@', line));
						for c in directive {
							finalcode.push((c as char, line));
						}
					},
				}
				false
			}
			'$' if variables.is_none() => {
				variables = Some(AHashMap::new());
				true
			}
			'\'' | '"' | '`' => {
				finalcode.push((c, line));
				let mut rawstring = Vec::new();
				while {
					code.read_until(c as u8, &mut rawstring)?;
					rawstring.len() >= 2 && rawstring[rawstring.len() - 2] == b'\\'
				} {}
				for c in Decoder::new(rawstring.into_iter()) {
					let c = c?;
					finalcode.push((c, line));
					if c == '\n' {
						line += 1;
					}
				}
				false
			}
			'/' => {
				if let Some(nc) = code.peek_char()? {
					match nc {
						'/' => {
							code.read_char().unwrap();
							code.read_line(&mut String::new())?;
							finalcode.push(('\n', line));
							line += 1;
							false
						}
						'*' => {
							code.read_char().unwrap();
							let mut newlines = Vec::new();
							while {
								code.read_until(b'*', &mut newlines)?;
								if let Some(fc) = code.read_char()? {
									fc != '/'
								} else {
									add_newlines(&mut finalcode, newlines, &mut line);
									return Err(analyze_error("Unterminated comment", line, filename))
								}
							} {}
							add_newlines(&mut finalcode, newlines, &mut line);
							false
						}
						_ => true
					}
				} else {
					true
				}
			}
			_ if c.is_ascii() => true,
			_ => {
				let mut buf = [0; 3];
				code.read(&mut buf)?;
				let buf = [c as u8, buf[0], buf[1], buf[2]];
				let c = decode(&mut buf.into_iter()).unwrap_or(Ok('�'))?;
				return Err(analyze_error(format!("Invalid character '{c}'"), line, filename))
			}
		} {
			finalcode.push((c, line))
		}
	}
	Ok((finalcode, variables))
}