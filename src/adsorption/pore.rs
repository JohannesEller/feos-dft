use crate::adsorption::{ExternalPotential, FluidParameters};
use crate::convolver::ConvolverFFT;
use crate::functional::{HelmholtzEnergyFunctional, DFT};
use crate::geometry::{Axis, AxisGeometry, Grid};
use crate::profile::{DFTProfile, CUTOFF_RADIUS, MAX_POTENTIAL};
use crate::solver::DFTSolver;
use feos_core::{Contributions, EosResult, EosUnit, State};
use ndarray::prelude::*;
use ndarray::Axis as Axis_nd;
use ndarray::Zip;
use ndarray_stats::QuantileExt;
use quantity::{QuantityArray2, QuantityScalar};
use std::rc::Rc;

const POTENTIAL_OFFSET: f64 = 2.0;
const DEFAULT_GRID_POINTS: usize = 2048;

/// Parameters required to specify a 1D pore.
pub struct Pore1D<U, F> {
    functional: Rc<DFT<F>>,
    geometry: AxisGeometry,
    pore_size: QuantityScalar<U>,
    potential: ExternalPotential<U>,
    n_grid: Option<usize>,
    potential_cutoff: Option<f64>,
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional> Pore1D<U, F> {
    pub fn new(
        functional: &Rc<DFT<F>>,
        geometry: AxisGeometry,
        pore_size: QuantityScalar<U>,
        potential: ExternalPotential<U>,
        n_grid: Option<usize>,
        potential_cutoff: Option<f64>,
    ) -> Self {
        Self {
            functional: functional.clone(),
            geometry,
            pore_size,
            potential,
            n_grid,
            potential_cutoff,
        }
    }
}

/// Parameters required to specify a 3D pore.
pub struct Pore3D<U, F> {
    functional: Rc<DFT<F>>,
    system_size: [QuantityScalar<U>; 3],
    n_grid: [usize; 3],
    coordinates: QuantityArray2<U>,
    sigma_ss: Array1<f64>,
    epsilon_k_ss: Array1<f64>,
    potential_cutoff: Option<f64>,
    cutoff_radius: Option<QuantityScalar<U>>,
}

impl<U, F> Pore3D<U, F> {
    pub fn new(
        functional: &Rc<DFT<F>>,
        system_size: [QuantityScalar<U>; 3],
        n_grid: [usize; 3],
        coordinates: QuantityArray2<U>,
        sigma_ss: Array1<f64>,
        epsilon_k_ss: Array1<f64>,
        potential_cutoff: Option<f64>,
        cutoff_radius: Option<QuantityScalar<U>>,
    ) -> Self {
        Self {
            functional: functional.clone(),
            system_size,
            n_grid,
            coordinates,
            sigma_ss,
            epsilon_k_ss,
            potential_cutoff,
            cutoff_radius,
        }
    }
}

/// Trait for the generic implementation of adsorption applications.
pub trait PoreSpecification<U, D: Dimension, F> {
    /// Initialize a new single pore.
    fn initialize(
        &self,
        bulk: &State<U, DFT<F>>,
        external_potential: Option<&Array<f64, D::Larger>>,
    ) -> EosResult<PoreProfile<U, D, F>>;
}

/// Density profile and properties of a confined system in arbitrary dimensions.
pub struct PoreProfile<U, D: Dimension, F> {
    pub profile: DFTProfile<U, D, F>,
    pub grand_potential: Option<QuantityScalar<U>>,
    pub interfacial_tension: Option<QuantityScalar<U>>,
}

/// Density profile and properties of a 1D confined system.
pub type PoreProfile1D<U, F> = PoreProfile<U, Ix1, F>;
/// Density profile and properties of a 3D confined system.
pub type PoreProfile3D<U, F> = PoreProfile<U, Ix3, F>;

impl<U: Copy, D: Dimension, F> Clone for PoreProfile<U, D, F> {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            grand_potential: self.grand_potential,
            interfacial_tension: self.interfacial_tension,
        }
    }
}

impl<U: EosUnit, D: Dimension, F: HelmholtzEnergyFunctional> PoreProfile<U, D, F>
where
    D::Larger: Dimension<Smaller = D>,
{
    pub fn solve_inplace(&mut self, solver: Option<&DFTSolver>, debug: bool) -> EosResult<()> {
        // Solve the profile
        self.profile.solve(solver, debug)?;

        // calculate grand potential density
        let omega = self
            .profile
            .integrate(&self.profile.dft.grand_potential_density(
                self.profile.temperature,
                &self.profile.density,
                &self.profile.convolver,
            )?);
        self.grand_potential = Some(omega);

        // calculate interfacial tension
        self.interfacial_tension =
            Some(omega + self.profile.bulk.pressure(Contributions::Total) * self.profile.volume());

        Ok(())
    }

    pub fn solve(mut self, solver: Option<&DFTSolver>) -> EosResult<Self> {
        self.solve_inplace(solver, false)?;
        Ok(self)
    }

    pub fn update_bulk(mut self, bulk: &State<U, DFT<F>>) -> Self {
        self.profile.bulk = bulk.clone();
        self.profile.chemical_potential = bulk.chemical_potential(Contributions::Total);
        self.grand_potential = None;
        self.interfacial_tension = None;
        self
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional + FluidParameters> PoreSpecification<U, Ix1, F>
    for Pore1D<U, F>
{
    fn initialize(
        &self,
        bulk: &State<U, DFT<F>>,
        external_potential: Option<&Array2<f64>>,
    ) -> EosResult<PoreProfile1D<U, F>> {
        let dft = &bulk.eos;
        let n_grid = self.n_grid.unwrap_or(DEFAULT_GRID_POINTS);

        let axis = match self.geometry {
            AxisGeometry::Cartesian => {
                let potential_offset =
                    POTENTIAL_OFFSET * self.functional.functional.sigma_ff().max().unwrap();
                Axis::new_cartesian(n_grid, 0.5 * self.pore_size, Some(potential_offset))?
            }
            AxisGeometry::Polar => Axis::new_polar(n_grid, self.pore_size)?,
            AxisGeometry::Spherical => Axis::new_spherical(n_grid, self.pore_size)?,
        };

        // calculate external potential
        let external_potential = external_potential.map_or_else(
            || {
                external_potential_1d(
                    self.pore_size,
                    bulk.temperature,
                    &self.potential,
                    &self.functional.functional,
                    &axis,
                    self.potential_cutoff,
                )
            },
            |e| Ok(e.clone()),
        )?;

        // initialize convolver
        let grid = Grid::new_1d(axis);
        let t = bulk.temperature.to_reduced(U::reference_temperature())?;
        let weight_functions = dft.functional.weight_functions(t);
        let convolver = ConvolverFFT::plan(&grid, &weight_functions, Some(1));

        Ok(PoreProfile {
            profile: DFTProfile::new(grid, convolver, bulk, Some(external_potential))?,
            grand_potential: None,
            interfacial_tension: None,
        })
    }
}

impl<U: EosUnit, F: HelmholtzEnergyFunctional, P: FluidParameters> PoreSpecification<U, Ix3, F>
    for Pore3D<U, P>
{
    fn initialize(
        &self,
        bulk: &State<U, DFT<F>>,
        external_potential: Option<&Array4<f64>>,
    ) -> EosResult<PoreProfile3D<U, F>> {
        let dft = &bulk.eos;

        // generate grid
        let x = Axis::new_cartesian(self.n_grid[0], self.system_size[0], None)?;
        let y = Axis::new_cartesian(self.n_grid[1], self.system_size[1], None)?;
        let z = Axis::new_cartesian(self.n_grid[2], self.system_size[2], None)?;

        // move center of geometry of solute to box center
        let coordinates = Array2::from_shape_fn(self.coordinates.raw_dim(), |(i, j)| {
            (self.coordinates.get((i, j)))
                .to_reduced(U::reference_length())
                .unwrap()
        });

        // temperature
        let t = bulk.temperature.to_reduced(U::reference_temperature())?;

        // calculate external potential
        let external_potential = external_potential.map_or_else(
            || {
                external_potential_3d(
                    &self.functional.functional,
                    [&x, &y, &z],
                    self.system_size,
                    coordinates,
                    &self.sigma_ss,
                    &self.epsilon_k_ss,
                    self.cutoff_radius,
                    self.potential_cutoff,
                    t,
                )
            },
            |e| Ok(e.clone()),
        )?;

        // initialize convolver
        let grid = Grid::Periodical3(x, y, z);
        let weight_functions = dft.functional.weight_functions(t);
        let convolver = ConvolverFFT::plan(&grid, &weight_functions, Some(1));

        Ok(PoreProfile {
            profile: DFTProfile::new(grid, convolver, bulk, Some(external_potential))?,
            grand_potential: None,
            interfacial_tension: None,
        })
    }
}

fn external_potential_1d<U: EosUnit, P: FluidParameters>(
    pore_width: QuantityScalar<U>,
    temperature: QuantityScalar<U>,
    potential: &ExternalPotential<U>,
    fluid_parameters: &P,
    axis: &Axis,
    potential_cutoff: Option<f64>,
) -> EosResult<Array2<f64>> {
    let potential_cutoff = potential_cutoff.unwrap_or(MAX_POTENTIAL);
    let effective_pore_size = match axis.geometry {
        AxisGeometry::Spherical => pore_width.to_reduced(U::reference_length())?,
        AxisGeometry::Polar => pore_width.to_reduced(U::reference_length())?,
        AxisGeometry::Cartesian => 0.5 * pore_width.to_reduced(U::reference_length())?,
    };
    let t = temperature.to_reduced(U::reference_temperature())?;
    let mut external_potential = match &axis.geometry {
        AxisGeometry::Cartesian => {
            potential.calculate_cartesian_potential(
                &(effective_pore_size + &axis.grid),
                fluid_parameters,
                t,
            ) + &potential.calculate_cartesian_potential(
                &(effective_pore_size - &axis.grid),
                fluid_parameters,
                t,
            )
        }
        AxisGeometry::Spherical => potential.calculate_spherical_potential(
            &axis.grid,
            effective_pore_size,
            fluid_parameters,
            t,
        ),
        AxisGeometry::Polar => potential.calculate_cylindrical_potential(
            &axis.grid,
            effective_pore_size,
            fluid_parameters,
            t,
        ),
    } / t;

    for (i, &z) in axis.grid.iter().enumerate() {
        if z > effective_pore_size {
            external_potential
                .index_axis_mut(Axis_nd(1), i)
                .fill(potential_cutoff);
        }
    }
    external_potential.map_inplace(|x| {
        if *x > potential_cutoff {
            *x = potential_cutoff
        }
    });
    Ok(external_potential)
}

pub fn external_potential_3d<U: EosUnit, F: FluidParameters>(
    functional: &F,
    axis: [&Axis; 3],
    system_size: [QuantityScalar<U>; 3],
    coordinates: Array2<f64>,
    sigma_ss: &Array1<f64>,
    epsilon_ss: &Array1<f64>,
    cutoff_radius: Option<QuantityScalar<U>>,
    potential_cutoff: Option<f64>,
    reduced_temperature: f64,
) -> EosResult<Array4<f64>> {
    // allocate external potential
    let m = functional.m();
    let mut external_potential = Array4::zeros((
        m.len(),
        axis[0].grid.len(),
        axis[1].grid.len(),
        axis[2].grid.len(),
    ));

    let system_size = [
        system_size[0].to_reduced(U::reference_length())?,
        system_size[1].to_reduced(U::reference_length())?,
        system_size[2].to_reduced(U::reference_length())?,
    ];

    let cutoff_radius = cutoff_radius
        .unwrap_or(CUTOFF_RADIUS * U::reference_length())
        .to_reduced(U::reference_length())?;

    // square cut-off radius
    let cutoff_radius2 = cutoff_radius.powi(2);

    // calculate external potential
    let sigma_ff = functional.sigma_ff();
    let epsilon_k_ff = functional.epsilon_k_ff();

    Zip::indexed(&mut external_potential).par_for_each(|(i, ix, iy, iz), u| {
        let distance2 = calculate_distance2(
            [&axis[0].grid[ix], &axis[1].grid[iy], &axis[2].grid[iz]],
            &coordinates,
            system_size,
        );
        let sigma_sf = sigma_ss.mapv(|s| (s + sigma_ff[i]) / 2.0);
        let epsilon_sf = epsilon_ss.mapv(|e| (e * epsilon_k_ff[i]).sqrt());
        *u = (0..sigma_ss.len())
            .map(|alpha| {
                m[i] * evaluate(
                    distance2[alpha],
                    sigma_sf[alpha],
                    epsilon_sf[alpha],
                    cutoff_radius2,
                )
            })
            .sum::<f64>()
            / reduced_temperature
    });

    let potential_cutoff = potential_cutoff.unwrap_or(MAX_POTENTIAL);
    external_potential.map_inplace(|x| {
        if *x > potential_cutoff {
            *x = potential_cutoff
        }
    });

    Ok(external_potential)
}

/// Evaluate LJ12-6 potential between solid site "alpha" and fluid segment
fn evaluate(distance2: f64, sigma: f64, epsilon: f64, cutoff_radius2: f64) -> f64 {
    let sigma_r = sigma.powi(2) / distance2;

    let potential: f64 = if distance2 > cutoff_radius2 {
        0.0
    } else if distance2 == 0.0 {
        f64::INFINITY
    } else {
        4.0 * epsilon * (sigma_r.powi(6) - sigma_r.powi(3))
    };

    potential
}

/// Evaluate the squared euclidian distance between a point and the coordinates of all solid atoms.
fn calculate_distance2(
    point: [&f64; 3],
    coordinates: &Array2<f64>,
    system_size: [f64; 3],
) -> Array1<f64> {
    Array1::from_shape_fn(coordinates.ncols(), |i| {
        let mut rx = coordinates[[0, i]] - point[0];
        let mut ry = coordinates[[1, i]] - point[1];
        let mut rz = coordinates[[2, i]] - point[2];

        rx -= system_size[0] * (rx / system_size[0]).round();
        ry -= system_size[1] * (ry / system_size[1]).round();
        rz -= system_size[2] * (rz / system_size[2]).round();

        rx.powi(2) + ry.powi(2) + rz.powi(2)
    })
}
