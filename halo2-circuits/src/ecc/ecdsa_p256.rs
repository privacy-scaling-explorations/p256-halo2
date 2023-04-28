#![allow(non_snake_case)]
use ark_std::{end_timer, start_timer};
use halo2_base::halo2_proofs::halo2curves::group::GroupEncoding;
use halo2_base::halo2_proofs::halo2curves::serde::SerdeObject;
use halo2_base::{utils::PrimeField, SKIP_FIRST_PASS};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::marker::PhantomData;
use std::{env::var, io::Write};

use halo2_base::halo2_proofs::{
    SerdeFormat,
    arithmetic::CurveAffine,
    circuit::{Value, Layouter, SimpleFloorPlanner},
    dev::MockProver,
    halo2curves::bn256::{Bn256, Fr, G1Affine},
    halo2curves::secp256r1::{Fp, Fq, Secp256r1Affine},
    plonk::*,
    poly::commitment::ParamsProver,
    transcript::{Blake2bRead, Blake2bWrite, Challenge255},
};
use rand_core::OsRng;

use halo2_ecc::{
    ecc::{ecdsa::ecdsa_verify_no_pubkey_check, EccChip},
    fields::{fp::{FpStrategy, FpConfig}, FieldChip},
};
use halo2_base::utils::{biguint_to_fe, fe_to_biguint, modulus};
use halo2_base::utils::fs::gen_srs;

type FpChip<F> = FpConfig<F, Fp>;

#[derive(Serialize, Deserialize)]
struct CircuitParams {
    strategy: FpStrategy,
    degree: u32,
    num_advice: usize,
    num_lookup_advice: usize,
    num_fixed: usize,
    lookup_bits: usize,
    limb_bits: usize,
    num_limbs: usize,
}

pub struct ECDSACircuit<F> {
    pub r: Option<Fq>,
    pub s: Option<Fq>,
    pub msghash: Option<Fq>,
    pub pk: Option<Secp256r1Affine>,
    pub G: Secp256r1Affine,
    pub _marker: PhantomData<F>,
}
impl<F: PrimeField> Default for ECDSACircuit<F> {
    fn default() -> Self {
        Self {
            r: None,
            s: None,
            msghash: None,
            pk: None,
            G: Secp256r1Affine::generator(),
            _marker: PhantomData,
        }
    }
}

impl<F: PrimeField> Circuit<F> for ECDSACircuit<F> {
    type Config = FpChip<F>;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        Self::default()
    }

    fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
        let path = var("ECDSA_CONFIG")
            .unwrap_or_else(|_| "./src/configs/ecdsa_circuit.config".to_string());
        let params: CircuitParams = serde_json::from_reader(
            File::open(&path).unwrap_or_else(|_| panic!("{path:?} file should exist")),
        )
        .unwrap();

        FpChip::<F>::configure(
            meta,
            params.strategy,
            &[params.num_advice],
            &[params.num_lookup_advice],
            params.num_fixed,
            params.lookup_bits,
            params.limb_bits,
            params.num_limbs,
            modulus::<Fp>(),
            0,
            params.degree as usize,
        )
    }

    fn synthesize(
        &self,
        fp_chip: Self::Config,
        mut layouter: impl Layouter<F>,
    ) -> Result<(), Error> {
        fp_chip.range.load_lookup_table(&mut layouter)?;

        let limb_bits = fp_chip.limb_bits;
        let num_limbs = fp_chip.num_limbs;
        let _num_fixed = fp_chip.range.gate.constants.len();
        let _lookup_bits = fp_chip.range.lookup_bits;
        let _num_advice = fp_chip.range.gate.num_advice;

        let mut first_pass = SKIP_FIRST_PASS;
        // ECDSA verify
        layouter.assign_region(
            || "ECDSA",
            |region| {
                if first_pass {
                    first_pass = false;
                    return Ok(());
                }

                let mut aux = fp_chip.new_context(region);
                let ctx = &mut aux;

                let (r_assigned, s_assigned, m_assigned) = {
                    let fq_chip = FpConfig::<F, Fq>::construct(
                        fp_chip.range.clone(),
                        limb_bits,
                        num_limbs,
                        modulus::<Fq>(),
                    );

                    let m_assigned = fq_chip.load_private(
                        ctx,
                        FpConfig::<F, Fq>::fe_to_witness(
                            &self.msghash.map_or(Value::unknown(), Value::known),
                        ),
                    );

                    let r_assigned = fq_chip.load_private(
                        ctx,
                        FpConfig::<F, Fq>::fe_to_witness(
                            &self.r.map_or(Value::unknown(), Value::known),
                        ),
                    );
                    let s_assigned = fq_chip.load_private(
                        ctx,
                        FpConfig::<F, Fq>::fe_to_witness(
                            &self.s.map_or(Value::unknown(), Value::known),
                        ),
                    );
                    (r_assigned, s_assigned, m_assigned)
                };

                let ecc_chip = EccChip::<F, FpChip<F>>::construct(fp_chip.clone());
                let pk_assigned = ecc_chip.load_private(
                    ctx,
                    (
                        self.pk.map_or(Value::unknown(), |pt| Value::known(pt.x)),
                        self.pk.map_or(Value::unknown(), |pt| Value::known(pt.y)),
                    ),
                );
                // test ECDSA
                let ecdsa = ecdsa_verify_no_pubkey_check::<F, Fp, Fq, Secp256r1Affine>(
                    &ecc_chip.field_chip,
                    ctx,
                    &pk_assigned,
                    &r_assigned,
                    &s_assigned,
                    &m_assigned,
                    4,
                    4,
                );

                // IMPORTANT: this copies cells to the lookup advice column to perform range check lookups
                // This is not optional.
                fp_chip.finalize(ctx);

                #[cfg(feature = "display")]
                if self.r.is_some() {
                    println!("ECDSA res {ecdsa:?}");

                    ctx.print_stats(&["Range"]);
                }
                Ok(())
            },
        )
    }
}

#[cfg(test)]
#[test]
fn test_secp256r1_ecdsa() {
    let mut folder = std::path::PathBuf::new();
    folder.push("./src");
    folder.push("./configs/ecdsa_circuit.config");
    println!("{:?}", folder.as_path().display());
    let params_str = std::fs::read_to_string(folder.as_path())
        .expect("src/configs/ecdsa_circuit.config file should exist");
    let params: CircuitParams = serde_json::from_str(params_str.as_str()).unwrap();
    let K = params.degree;

    // generate random pub key and sign random message
    let G = Secp256r1Affine::generator();
    let sk = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
    let pubkey = Secp256r1Affine::from(G * sk);
    let msg_hash = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);

    let k = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
    let k_inv = k.invert().unwrap();

    let r_point = Secp256r1Affine::from(G * k).coordinates().unwrap();
    let x = r_point.x();
    let x_bigint = fe_to_biguint(x);
    let r = biguint_to_fe::<Fq>(&(x_bigint % modulus::<Fq>()));
    let s = k_inv * (msg_hash + (r * sk));

    let circuit = ECDSACircuit::<Fr> {
        r: Some(r),
        s: Some(s),
        msghash: Some(msg_hash),
        pk: Some(pubkey),
        G,
        _marker: PhantomData,
    };

    let prover = MockProver::run(K, &circuit, vec![]).unwrap();
    //prover.assert_satisfied();
    assert_eq!(prover.verify(), Ok(()));
}

// fn generate_keys<F>() -> Result<(ProvingKey<F>, VerifyingKey<F>)> {
//     let vk = keygen_vk(&params, &circuit)?;
//     let pk = keygen_pk(&params, vk, &circuit)?;
//     Ok((pk, vk))
// }

pub fn download_keys(degree: u32, proving_key_path: Option<&str>, verifying_key_path: Option<&str>) -> Result<(), Error> {
    let circuit = ECDSACircuit::<Fr>::default();
    let params = gen_srs(degree);
    let vk = keygen_vk(&params, &circuit)?;
    let pk = keygen_pk(&params, vk.clone(), &circuit)?;
    proving_key_path.map(|path| {
        let mut file = File::create(path).expect("Unable to create proving key file");
        let pk_bytes = pk.to_bytes(SerdeFormat::RawBytes);
        file.write_all(&pk_bytes).unwrap()
    });
    verifying_key_path.map(|path| {
        let mut file = File::create(path).expect("Unable to create verifying key file");
        let vk_bytes = vk.to_bytes(SerdeFormat::RawBytes);
        file.write_all(&vk_bytes).unwrap()
    });
    Ok(())
}

pub fn generate_proof(pubkey_x: &[u8; 32], pubkey_y: &[u8; 32], r: &[u8; 32], s: &[u8; 32], msg_hash: &[u8; 32], proving_key_path: &str, degree: u32) -> Result<Vec<u8>, Error> {
    use halo2_base::halo2_proofs::{
        poly::kzg::{
            commitment::KZGCommitmentScheme,
            multiopen::{ProverSHPLONK, VerifierSHPLONK},
            strategy::SingleStrategy,
        },
        transcript::{TranscriptReadBuffer, TranscriptWriterBuffer},
    };
    let params = gen_srs(degree);
    println!("0");
    let proving_key = {
        let f = File::open(proving_key_path).expect("Unable to open proving key file");
        let mut reader = BufReader::new(f);
        ProvingKey::<G1Affine>::read::<_, ECDSACircuit<Fr>>(&mut reader, SerdeFormat::RawBytes)

    }?;
    println!("1");
    //start paste
    // let G = Secp256r1Affine::generator();
    // // let sk = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
    // let sk = <Secp256r1Affine as CurveAffine>::ScalarExt::from(10);
    // let pubkey = Secp256r1Affine::from(G * sk);
    // let msg_hash = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);

    // let k = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
    // let k_inv = k.invert().unwrap();

    // let r_point = Secp256r1Affine::from(G * k).coordinates().unwrap();
    // let x = r_point.x();
    // let x_bigint = fe_to_biguint(x);
    // let r = biguint_to_fe::<Fq>(&x_bigint);
    // let s = k_inv * (msg_hash + (r * sk));

    // let proof_circuit = ECDSACircuit::<Fr> {
    //     r: Some(r),
    //     s: Some(s),
    //     msghash: Some(msg_hash),
    //     pk: Some(pubkey),
    //     G,
    //     _marker: PhantomData,
    // };
    // end paste

    let G = Secp256r1Affine::generator();
    println!("raw_inputs:");
    println!("pubkey_x: {:?}", pubkey_x);
    println!("pubkey_y: {:?}", pubkey_y);
    println!("msg_hash: {:?}", msg_hash);
    println!("r: {:?}", r);
    println!("s: {:?}", s);
    
    // Fp::from_bytes(bytes);
    let pubkey_x_base = Fp::from_bytes(pubkey_x).unwrap();
    let pubkey_y_base = Fp::from_bytes(pubkey_y).unwrap();
    let pubkey_point = Secp256r1Affine::from_xy(pubkey_x_base, pubkey_y_base).into();
    let msghash = <Secp256r1Affine as CurveAffine>::ScalarExt::from_bytes(msg_hash).into();
    // let msghash  = Fq::from_bytes(msg_hash).into();

    let r_point = <Secp256r1Affine as CurveAffine>::ScalarExt::from_bytes(r).into();
    let s_point = <Secp256r1Affine as CurveAffine>::ScalarExt::from_bytes(s).into();

    println!("generated_points:");
    println!("pubkey_point: {:?}", pubkey_point);
    println!("msghash: {:?}", msghash);
    println!("r_point: {:?}", r_point);
    println!("s_point: {:?}", s_point);
    println!("Fpsize: {:?}", Fp::size());

    let proof_circuit = ECDSACircuit::<Fr> {
        r: r_point,
        s: s_point,
        msghash,
        pk: pubkey_point,
        G,
        _marker: PhantomData,
    };
    println!("3");
    // println!("{:?}", params);
    let mut rng = OsRng;

    // create a proof
    let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
    println!("4");
    create_proof::<
        KZGCommitmentScheme<Bn256>,
        ProverSHPLONK<'_, Bn256>,
        Challenge255<G1Affine>,
        _,
        Blake2bWrite<Vec<u8>, G1Affine, Challenge255<G1Affine>>,
        ECDSACircuit<Fr>,
    >(&params, &proving_key, &[proof_circuit], &[&[]], &mut rng, &mut transcript)?;
    println!("5");

    let proof = transcript.finalize();
    Ok (proof)
}

#[cfg(test)]
#[test]
fn bench_secp256r1_ecdsa() -> Result<(), Box<dyn std::error::Error>> {
    use halo2_base::utils::fs::gen_srs;

    use halo2_base::halo2_proofs::{
        poly::kzg::{
            commitment::KZGCommitmentScheme,
            multiopen::{ProverSHPLONK, VerifierSHPLONK},
            strategy::SingleStrategy,
        },
        transcript::{TranscriptReadBuffer, TranscriptWriterBuffer},
    };
    use std::{env::set_var, fs, io::BufRead};

    let _rng = OsRng;

    let mut folder = std::path::PathBuf::new();
    folder.push("./src");

    folder.push("configs/bench_ecdsa.config");
    let bench_params_file = std::fs::File::open(folder.as_path()).unwrap();
    folder.pop();
    folder.pop();

    folder.push("results/ecdsa_bench.csv");
    let mut fs_results = std::fs::File::create(folder.as_path()).unwrap();
    folder.pop();
    folder.pop();
    writeln!(fs_results, "degree,num_advice,num_lookup,num_fixed,lookup_bits,limb_bits,num_limbs,proof_time,proof_size,verify_time")?;
    folder.push("data");
    if !folder.is_dir() {
        std::fs::create_dir(folder.as_path())?;
    }

    let bench_params_reader = std::io::BufReader::new(bench_params_file);
    for line in bench_params_reader.lines() {
        let bench_params: CircuitParams = serde_json::from_str(line.unwrap().as_str()).unwrap();
        println!(
            "---------------------- degree = {} ------------------------------",
            bench_params.degree
        );

        {
            folder.pop();
            folder.push("configs/ecdsa_circuit.tmp.config");
            set_var("ECDSA_CONFIG", &folder);
            let mut f = std::fs::File::create(folder.as_path())?;
            write!(f, "{}", serde_json::to_string(&bench_params).unwrap())?;
            folder.pop();
            folder.pop();
            folder.push("data");
        }
        let params_time = start_timer!(|| "Time elapsed in circuit & params construction");
        let params = gen_srs(bench_params.degree);
        let circuit = ECDSACircuit::<Fr>::default();
        end_timer!(params_time);

        let vk_time = start_timer!(|| "Time elapsed in generating vkey");
        let vk = keygen_vk(&params, &circuit)?;
        end_timer!(vk_time);

        let pk_time = start_timer!(|| "Time elapsed in generating pkey");
        let pk = keygen_pk(&params, vk, &circuit)?;
        end_timer!(pk_time);

        // generate random pub key and sign random message
        let G = Secp256r1Affine::generator();
        let sk = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
        let pubkey = Secp256r1Affine::from(G * sk);
        let msg_hash = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);

        let k = <Secp256r1Affine as CurveAffine>::ScalarExt::random(OsRng);
        let k_inv = k.invert().unwrap();

        let r_point = Secp256r1Affine::from(G * k).coordinates().unwrap();
        let x = r_point.x();
        let x_bigint = fe_to_biguint(x);
        let r = biguint_to_fe::<Fq>(&x_bigint);
        let s = k_inv * (msg_hash + (r * sk));

        let proof_circuit = ECDSACircuit::<Fr> {
            r: Some(r),
            s: Some(s),
            msghash: Some(msg_hash),
            pk: Some(pubkey),
            G,
            _marker: PhantomData,
        };
        let mut rng = OsRng;

        // create a proof
        let proof_time = start_timer!(|| "Proving time");
        let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
        create_proof::<
            KZGCommitmentScheme<Bn256>,
            ProverSHPLONK<'_, Bn256>,
            Challenge255<G1Affine>,
            _,
            Blake2bWrite<Vec<u8>, G1Affine, Challenge255<G1Affine>>,
            ECDSACircuit<Fr>,
        >(&params, &pk, &[proof_circuit], &[&[]], &mut rng, &mut transcript)?;
        let proof = transcript.finalize();
        end_timer!(proof_time);

        let proof_size = {
            folder.push(format!(
                "ecdsa_circuit_proof_{}_{}_{}_{}_{}_{}_{}.data",
                bench_params.degree,
                bench_params.num_advice,
                bench_params.num_lookup_advice,
                bench_params.num_fixed,
                bench_params.lookup_bits,
                bench_params.limb_bits,
                bench_params.num_limbs
            ));
            let mut fd = std::fs::File::create(folder.as_path()).unwrap();
            folder.pop();
            fd.write_all(&proof).unwrap();
            fd.metadata().unwrap().len()
        };

        let verify_time = start_timer!(|| "Verify time");
        let verifier_params = params.verifier_params();
        let strategy = SingleStrategy::new(&params);
        let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(&proof[..]);
        assert!(verify_proof::<
            KZGCommitmentScheme<Bn256>,
            VerifierSHPLONK<'_, Bn256>,
            Challenge255<G1Affine>,
            Blake2bRead<&[u8], G1Affine, Challenge255<G1Affine>>,
            SingleStrategy<'_, Bn256>,
        >(verifier_params, pk.get_vk(), strategy, &[&[]], &mut transcript)
        .is_ok());
        end_timer!(verify_time);
        fs::remove_file(var("ECDSA_CONFIG").unwrap())?;

        writeln!(
            fs_results,
            "{},{},{},{},{},{},{},{:?},{},{:?}",
            bench_params.degree,
            bench_params.num_advice,
            bench_params.num_lookup_advice,
            bench_params.num_fixed,
            bench_params.lookup_bits,
            bench_params.limb_bits,
            bench_params.num_limbs,
            proof_time.time.elapsed(),
            proof_size,
            verify_time.time.elapsed()
        )?;
    }
    Ok(())
}
